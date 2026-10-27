//! Fakes shared by the tab tests: OS ports that answer from a table and log
//! every call, a scripted terminal backend and a scripted agent.

use super::process::Process;
use super::{AgentInfo, CommandRunner, OsaRunner, Ports, Sleeper, Tab, TerminalBackend, TtyWriter};
use crate::agent::{Agent, AgentContext};
use crate::error::{Error, Result};
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

/// A call log shared by several fakes, so tests can check the order of calls
/// across them.
pub(crate) type Log = Rc<RefCell<Vec<String>>>;

/// A new, empty shared log.
pub(crate) fn log() -> Log {
    Rc::new(RefCell::new(Vec::new()))
}

/// OS ports. `run` answers by `"<program> <first arg>"` from `answers`; a
/// missing key is an error. Every call is logged.
pub(crate) struct FakeOs {
    pub log: Log,
    pub answers: HashMap<String, std::result::Result<String, String>>,
}

impl FakeOs {
    pub fn new(log: &Log) -> Self {
        FakeOs {
            log: log.clone(),
            answers: HashMap::new(),
        }
    }

    /// Answer `key` (`"<program> <first arg>"`) with `out`.
    pub fn answer(mut self, key: &str, out: &str) -> Self {
        self.answers.insert(key.into(), Ok(out.into()));
        self
    }

    /// Fail `key` with `err`.
    pub fn fail(mut self, key: &str, err: &str) -> Self {
        self.answers.insert(key.into(), Err(err.into()));
        self
    }

    pub fn ports(&self) -> Ports<'_> {
        Ports {
            osa: self,
            cmd: self,
            tty: self,
            sleep: self,
        }
    }
}

impl CommandRunner for FakeOs {
    fn run(&self, program: &str, args: &[&str]) -> Result<String> {
        self.log
            .borrow_mut()
            .push(format!("run:{program} {}", args.join(" ")));
        let key = format!("{program} {}", args.first().copied().unwrap_or(""));
        match self.answers.get(&key) {
            Some(Ok(out)) => Ok(out.clone()),
            Some(Err(e)) => Err(Error::other(e.clone())),
            None => Err(Error::other(format!("no fake answer for {key}"))),
        }
    }
}

impl OsaRunner for FakeOs {
    fn run_jxa(&self, _script: &str) -> Result<String> {
        self.log.borrow_mut().push("osa".into());
        Ok(String::new())
    }
}

impl TtyWriter for FakeOs {
    fn write(&self, tty: &str, _bytes: &[u8]) -> Result<()> {
        self.log.borrow_mut().push(format!("tty:{tty}"));
        Ok(())
    }
}

impl Sleeper for FakeOs {
    fn sleep_ms(&self, ms: u64) {
        self.log.borrow_mut().push(format!("sleep:{ms}"));
    }
}

/// What a [`FakeBackend`] returns from `list`.
#[derive(Clone)]
pub(crate) enum Listing {
    Tabs(Vec<Tab>),
    NotRunning,
    Fail(&'static str),
}

/// A terminal backend that lists fixed tabs and shows a set of ttys. Focus
/// calls are logged as `focus:<id>:<ref>` and `focus_tty:<id>:<tty>`.
pub(crate) struct FakeBackend {
    pub id: &'static str,
    pub listing: Listing,
    pub shows: Vec<&'static str>,
    pub tty_error: bool,
    pub log: Log,
}

impl FakeBackend {
    pub fn new(id: &'static str, log: &Log) -> Self {
        FakeBackend {
            id,
            listing: Listing::NotRunning,
            shows: Vec::new(),
            tty_error: false,
            log: log.clone(),
        }
    }

    pub fn tabs(mut self, tabs: Vec<Tab>) -> Self {
        self.listing = Listing::Tabs(tabs);
        self
    }

    pub fn failing(mut self, msg: &'static str) -> Self {
        self.listing = Listing::Fail(msg);
        self
    }

    /// This backend has a tab on `tty`.
    pub fn shows(mut self, tty: &'static str) -> Self {
        self.shows.push(tty);
        self
    }

    /// `focus_tty` returns an error.
    pub fn tty_error(mut self) -> Self {
        self.tty_error = true;
        self
    }

    pub fn boxed(self) -> Box<dyn TerminalBackend> {
        Box::new(self)
    }
}

impl TerminalBackend for FakeBackend {
    fn id(&self) -> &'static str {
        self.id
    }

    fn list(&self, _ports: &Ports) -> Result<Option<Vec<Tab>>> {
        match self.listing.clone() {
            Listing::Tabs(t) => Ok(Some(t)),
            Listing::NotRunning => Ok(None),
            Listing::Fail(m) => Err(Error::other(m)),
        }
    }

    fn focus(&self, _ports: &Ports, tab_ref: &str) -> Result<()> {
        self.log
            .borrow_mut()
            .push(format!("focus:{}:{tab_ref}", self.id));
        Ok(())
    }

    fn focus_tty(&self, _ports: &Ports, tty: &str) -> Result<bool> {
        self.log
            .borrow_mut()
            .push(format!("focus_tty:{}:{tty}", self.id));
        if self.tty_error {
            return Err(Error::other("refused"));
        }
        Ok(self.shows.contains(&tty))
    }
}

/// A tab of `terminal` with a ref, title and position.
pub(crate) fn tab(terminal: &str, ref_: &str, title: &str, window: u32, index: u32) -> Tab {
    Tab {
        terminal: terminal.into(),
        ref_: Some(ref_.into()),
        title: title.into(),
        window,
        index,
        ..Tab::default()
    }
}

/// The same tab with a tty.
pub(crate) fn with_tty(mut t: Tab, tty: &str) -> Tab {
    t.tty = Some(tty.into());
    t
}

/// An agent that matches the processes whose pid it has an info for, and
/// returns that info with the process tty filled in.
pub(crate) struct FakeAgent {
    pub infos: Vec<AgentInfo>,
}

impl Agent for FakeAgent {
    fn tool(&self) -> &'static str {
        "fake"
    }

    fn matches(&self, p: &Process) -> bool {
        self.infos.iter().any(|i| i.pid == p.pid)
    }

    fn enrich(&self, p: &Process, _ctx: &AgentContext) -> AgentInfo {
        let mut info = self
            .infos
            .iter()
            .find(|i| i.pid == p.pid)
            .cloned()
            .unwrap_or_default();
        info.tty = p.tty.clone();
        info
    }
}

/// An agent info for `pid` with a title and update time.
pub(crate) fn agent(pid: u32, title: Option<&str>, updated_at: Option<i64>) -> AgentInfo {
    AgentInfo {
        tool: "claude-code".into(),
        pid,
        title: title.map(str::to_string),
        updated_at,
        ..AgentInfo::default()
    }
}

/// One `ps` line (in the `PS_ARGS` layout) for a foreground process.
pub(crate) fn ps_line(pid: u32, tty: &str) -> String {
    format!("{pid} 1 {pid} {pid} {tty} 00:01 agent\n")
}

/// An agent context that points nowhere.
pub(crate) fn ctx() -> AgentContext {
    AgentContext {
        home: PathBuf::from("/nonexistent"),
        claude_dirs: vec![],
        codex_home: PathBuf::from("/nonexistent/codex"),
        now_ms: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_os_logs_and_answers() {
        let l = log();
        let os = FakeOs::new(&l).answer("ps -axo", "x").fail("tmux ls", "no");
        let p = os.ports();
        assert_eq!(p.cmd.run("ps", &["-axo"]).unwrap(), "x");
        assert_eq!(p.cmd.run("tmux", &["ls"]).unwrap_err().to_string(), "no");
        assert!(p.cmd.run("kitten", &[]).is_err());
        assert!(p.osa.run_jxa("s").unwrap().is_empty());
        p.tty.write("ttys1", b"x").unwrap();
        p.sleep.sleep_ms(5);
        assert_eq!(
            *l.borrow(),
            vec![
                "run:ps -axo",
                "run:tmux ls",
                "run:kitten ",
                "osa",
                "tty:ttys1",
                "sleep:5"
            ]
        );
    }

    #[test]
    fn fake_agent_matches_by_pid_and_fills_tty() {
        let a = FakeAgent {
            infos: vec![agent(7, Some("t"), None)],
        };
        assert_eq!(a.tool(), "fake");
        let p = super::super::process::ProcessTable::parse(&ps_line(7, "ttys3"));
        assert!(a.matches(&p.procs[0]));
        let info = a.enrich(&p.procs[0], &ctx());
        assert_eq!(info.tty.as_deref(), Some("ttys3"));
        let other = super::super::process::ProcessTable::parse(&ps_line(8, "ttys3"));
        assert!(!a.matches(&other.procs[0]));
        assert_eq!(a.enrich(&other.procs[0], &ctx()).pid, 0);
    }
}
