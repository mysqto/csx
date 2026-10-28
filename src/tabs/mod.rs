//! Terminal tabs and the AI-tool agents running in them: the shared model, the
//! OS ports every backend consumes, and row filtering/sorting.

pub mod app;
pub mod closed;
pub mod ghostty;
pub mod iterm2;
pub mod kitty;
pub mod open;
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

/// An indexed session that no detected agent runs, and how to resume it.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct ClosedSession {
    /// Tool id: `claude-code`, `codex` or `devin`.
    pub tool: String,
    /// Session id.
    pub session_id: String,
    /// Session title, else the first user prompt, else the project name.
    pub title: Option<String>,
    /// The folder the session ran in.
    pub cwd: String,
    /// Last user prompt, when known.
    pub last_prompt: Option<String>,
    /// Last activity time in unix milliseconds.
    pub updated_at: Option<i64>,
    /// `cd '<cwd>' && <command>`, which resumes the session from any folder.
    pub resume_command: String,
}

/// The result of listing tabs.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct TabsReport {
    /// Rows, sorted for display.
    pub rows: Vec<Row>,
    /// Non-fatal problems met while listing.
    pub warnings: Vec<String>,
    /// Closed sessions, newest first; `None` when they were not asked for.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub closed: Option<Vec<ClosedSession>>,
    /// Session ids of every detected agent, before any filter.
    #[serde(skip)]
    pub live_sessions: Vec<String>,
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

/// A terminal that `csx tabs open` can open a tab in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct TerminalInfo {
    /// The id `--terminal` takes, for example `ghostty`.
    pub id: &'static str,
    /// Display name, for example `Ghostty`.
    pub name: &'static str,
    /// macOS bundle id, used to find and start the app.
    #[serde(skip)]
    pub bundle_id: &'static str,
}

/// Ghostty.
pub const GHOSTTY: TerminalInfo = TerminalInfo {
    id: "ghostty",
    name: "Ghostty",
    bundle_id: "com.mitchellh.ghostty",
};
/// iTerm2.
pub const ITERM2: TerminalInfo = TerminalInfo {
    id: "iterm2",
    name: "iTerm2",
    bundle_id: "com.googlecode.iterm2",
};
/// Terminal, which ships with macOS.
pub const TERMINAL: TerminalInfo = TerminalInfo {
    id: "terminal",
    name: "Terminal",
    bundle_id: "com.apple.Terminal",
};
/// kitty.
pub const KITTY: TerminalInfo = TerminalInfo {
    id: "kitty",
    name: "kitty",
    bundle_id: "net.kovidgoyal.kitty",
};
/// WezTerm.
pub const WEZTERM: TerminalInfo = TerminalInfo {
    id: "wezterm",
    name: "WezTerm",
    bundle_id: "com.github.wez.wezterm",
};

/// Every terminal `csx tabs open` supports, in the order pickers show them.
pub const TERMINALS: &[TerminalInfo] = &[GHOSTTY, ITERM2, TERMINAL, KITTY, WEZTERM];

/// The terminals of [`TERMINALS`] that are installed, in that order. A
/// terminal is installed when Spotlight finds an app with its bundle id.
/// Terminal ships with macOS and always counts.
pub fn installed_terminals(cmd: &dyn CommandRunner) -> Vec<&'static TerminalInfo> {
    TERMINALS
        .iter()
        .filter(|t| {
            t.id == TERMINAL.id
                || cmd
                    .run(
                        "mdfind",
                        &[&format!("kMDItemCFBundleIdentifier == '{}'", t.bundle_id)],
                    )
                    .is_ok_and(|out| !out.trim().is_empty())
        })
        .collect()
}

/// How many times csx checks that a terminal it started is ready.
pub const LAUNCH_POLLS: u32 = 20;
/// Time between two of those checks, in milliseconds.
pub const LAUNCH_POLL_MS: u64 = 250;

/// True when the app named `app` runs, asked through JXA. A failed run
/// counts as not running.
pub fn app_running(ports: &Ports, app: &str) -> bool {
    let script = format!(
        r#"(() => {{ try {{ return String(Application({}).running()); }} catch (e) {{ return "false"; }} }})()"#,
        serde_json::to_string(app).unwrap_or_default()
    );
    ports
        .osa
        .run_jxa(&script)
        .is_ok_and(|out| out.trim() == "true")
}

/// Start the terminal with `open -b <bundle id>`, then check `ready` up to
/// [`LAUNCH_POLLS`] times, [`LAUNCH_POLL_MS`] apart. Error `<name> did not
/// start` when it never gets ready.
pub fn launch(ports: &Ports, info: &TerminalInfo, ready: &dyn Fn(&Ports) -> bool) -> Result<()> {
    ports.cmd.run("open", &["-b", info.bundle_id])?;
    for _ in 0..LAUNCH_POLLS {
        ports.sleep.sleep_ms(LAUNCH_POLL_MS);
        if ready(ports) {
            return Ok(());
        }
    }
    Err(crate::error::Error::other(format!(
        "{} did not start",
        info.name
    )))
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
    /// Open a new tab in `cwd`, type `line` and a newline into it, and bring
    /// the app to the front. Starts the app when it is not running.
    fn open_tab(&self, _ports: &Ports, _cwd: &str, _line: &str) -> Result<()> {
        Err(crate::error::Error::other(format!(
            "{} cannot open tabs",
            self.id()
        )))
    }
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
    words_match(&parts, words)
}

/// True when every word (compared case-insensitively) appears in `parts`
/// joined by spaces. No words match anything.
pub fn words_match(parts: &[&str], words: &[String]) -> bool {
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

/// Sort rows: agent rows first, newest `updated_at` first (`None` last), ties
/// by (terminal, window, index, agent pid); then tabs by (terminal, window,
/// index).
pub fn sort_rows(rows: &mut [Row]) {
    rows.sort_by(|a, b| match (&a.agent, &b.agent) {
        (Some(x), Some(y)) => y.updated_at.cmp(&x.updated_at).then_with(|| {
            (&a.terminal, a.window, a.index, x.pid).cmp(&(&b.terminal, b.window, b.index, y.pid))
        }),
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
    fn words_match_needs_every_word_across_the_joined_parts() {
        assert!(words_match(&["Fix", "the PARSER"], &words("parser fix")));
        // Words may span two parts, which are joined by a space.
        assert!(words_match(&["fix", "the"], &words("x t")));
        assert!(!words_match(&["fix", "the"], &words("fixthe")));
        assert!(!words_match(&["fix"], &words("fix missing")));
        assert!(words_match(&[], &[]));
        assert!(!words_match(&[], &words("a")));
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
    fn sort_rows_breaks_agent_ties_by_position_then_pid() {
        let at = |terminal: &str, window: u32, index: u32, pid: u32, updated: Option<i64>| {
            let mut r = tab(terminal, window, index, "");
            r.agent = Some(AgentInfo {
                pid,
                updated_at: updated,
                ..AgentInfo::default()
            });
            r
        };
        let mut rows = vec![
            at("tmux", 1, 1, 1, Some(5)),
            at("", 0, 0, 9, None),
            at("ghostty", 1, 2, 2, Some(5)),
            at("", 0, 0, 3, None),
            at("ghostty", 1, 1, 7, Some(5)),
            at("ghostty", 1, 1, 4, Some(5)),
            at("iterm2", 1, 1, 8, Some(6)),
        ];
        sort_rows(&mut rows);
        let pids: Vec<u32> = rows.iter().map(|r| r.agent.as_ref().unwrap().pid).collect();
        assert_eq!(pids, vec![8, 4, 7, 2, 1, 3, 9]);
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

    use crate::tabs::test_support::{log, FakeOs};
    use std::cell::Cell;

    #[test]
    fn terminals_in_target_order_and_json_without_bundle_id() {
        let ids: Vec<&str> = TERMINALS.iter().map(|t| t.id).collect();
        assert_eq!(ids, ["ghostty", "iterm2", "terminal", "kitty", "wezterm"]);
        let names: Vec<&str> = TERMINALS.iter().map(|t| t.name).collect();
        assert_eq!(names, ["Ghostty", "iTerm2", "Terminal", "kitty", "WezTerm"]);
        let bundles: Vec<&str> = TERMINALS.iter().map(|t| t.bundle_id).collect();
        assert_eq!(
            bundles,
            [
                "com.mitchellh.ghostty",
                "com.googlecode.iterm2",
                "com.apple.Terminal",
                "net.kovidgoyal.kitty",
                "com.github.wez.wezterm"
            ]
        );
        assert_eq!(
            serde_json::to_string(&TERMINALS[..2]).unwrap(),
            r#"[{"id":"ghostty","name":"Ghostty"},{"id":"iterm2","name":"iTerm2"}]"#
        );
    }

    #[test]
    fn installed_terminals_asks_spotlight_per_bundle_id() {
        let q = |b: &str| format!("mdfind kMDItemCFBundleIdentifier == '{b}'");
        let l = log();
        let os = FakeOs::new(&l)
            .answer(&q("com.mitchellh.ghostty"), "/Applications/Ghostty.app\n")
            .answer(&q("com.googlecode.iterm2"), " \n")
            .fail(&q("net.kovidgoyal.kitty"), "mdfind failed")
            .answer(&q("com.github.wez.wezterm"), "/Applications/WezTerm.app\n");
        let got: Vec<&str> = installed_terminals(&os).iter().map(|t| t.id).collect();
        assert_eq!(got, ["ghostty", "terminal", "wezterm"]);
        // Terminal ships with macOS: it needs no lookup.
        assert_eq!(
            *l.borrow(),
            [
                "run:mdfind kMDItemCFBundleIdentifier == 'com.mitchellh.ghostty'",
                "run:mdfind kMDItemCFBundleIdentifier == 'com.googlecode.iterm2'",
                "run:mdfind kMDItemCFBundleIdentifier == 'net.kovidgoyal.kitty'",
                "run:mdfind kMDItemCFBundleIdentifier == 'com.github.wez.wezterm'",
            ]
        );
    }

    #[test]
    fn open_tab_defaults_to_an_error() {
        struct Bare;
        impl TerminalBackend for Bare {
            fn id(&self) -> &'static str {
                "bare"
            }
            fn list(&self, _: &Ports) -> Result<Option<Vec<Tab>>> {
                Ok(None)
            }
            fn focus(&self, _: &Ports, _: &str) -> Result<()> {
                Ok(())
            }
            fn focus_tty(&self, _: &Ports, _: &str) -> Result<bool> {
                Ok(false)
            }
        }
        let os = FakeOs::new(&log());
        let p = os.ports();
        assert!(Bare.list(&p).unwrap().is_none());
        Bare.focus(&p, "x").unwrap();
        assert!(!Bare.focus_tty(&p, "ttys1").unwrap());
        let e = Bare.open_tab(&p, "/x", "y").unwrap_err();
        assert_eq!(e.to_string(), "bare cannot open tabs");
    }

    #[test]
    fn app_running_reads_the_jxa_answer() {
        let os = FakeOs::new(&log())
            .osa("true\n")
            .osa("false")
            .osa_fail("boom");
        assert!(app_running(&os.ports(), "Ghostty"));
        assert!(!app_running(&os.ports(), "Ghostty"));
        assert!(!app_running(&os.ports(), "Ghostty"));
        let s = os.scripts.borrow()[0].clone();
        assert!(s.contains(r#"String(Application("Ghostty").running())"#));
        assert!(s.contains("catch (e) { return \"false\"; }"));
    }

    #[test]
    fn launch_opens_the_bundle_then_polls_until_ready() {
        let l = log();
        let os = FakeOs::new(&l).answer("open -b", "");
        let polls = Cell::new(0);
        let ready = |_: &Ports| {
            polls.set(polls.get() + 1);
            polls.get() == 3
        };
        launch(&os.ports(), &GHOSTTY, &ready).unwrap();
        assert_eq!(
            *l.borrow(),
            [
                "run:open -b com.mitchellh.ghostty",
                "sleep:250",
                "sleep:250",
                "sleep:250"
            ]
        );
    }

    #[test]
    fn launch_gives_up_after_twenty_polls() {
        let l = log();
        let os = FakeOs::new(&l).answer("open -b", "");
        let e = launch(&os.ports(), &WEZTERM, &|_| false).unwrap_err();
        assert_eq!(e.to_string(), "WezTerm did not start");
        let sleeps = l.borrow().iter().filter(|c| *c == "sleep:250").count();
        assert_eq!(sleeps, LAUNCH_POLLS as usize);
    }

    #[test]
    fn launch_reports_an_open_failure() {
        let l = log();
        let os = FakeOs::new(&l).fail("open -b", "open failed: no app");
        let e = launch(&os.ports(), &ITERM2, &|_| true).unwrap_err();
        assert_eq!(e.to_string(), "open failed: no app");
        assert_eq!(l.borrow().len(), 1);
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
