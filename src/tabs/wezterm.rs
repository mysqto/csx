//! WezTerm terminal backend, built from documentation: `wezterm cli list
//! --format json` lists panes and `wezterm cli activate-pane` focuses one.

use super::{Ports, Tab, TerminalBackend};
use crate::error::{Error, Result};
use serde::Deserialize;

const REF_PREFIX: &str = "wezterm:";

/// The WezTerm backend.
pub struct WezTerm;

#[derive(Deserialize)]
struct RawPane {
    window_id: u64,
    tab_id: u64,
    pane_id: u64,
    #[serde(default)]
    title: String,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    tty_name: Option<String>,
}

struct Pane {
    window: u32,
    index: u32,
    pane_id: u64,
    title: String,
    cwd: Option<String>,
    tty: Option<String>,
}

/// Decode `%XX` escapes; an invalid escape is left as-is.
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = b
            .get(i + 1..i + 3)
            .and_then(|h| std::str::from_utf8(h).ok())
            .and_then(|h| u8::from_str_radix(h, 16).ok());
        match (b[i], hex) {
            (b'%', Some(v)) => {
                out.push(v);
                i += 3;
            }
            (c, _) => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Strip `file://<host>` from a WezTerm cwd and decode the path.
fn strip_file_url(cwd: &str) -> String {
    match cwd.strip_prefix("file://") {
        Some(rest) => percent_decode(rest.find('/').map_or("", |i| &rest[i..])),
        None => cwd.to_string(),
    }
}

fn parse_panes(json: &str) -> Result<Vec<Pane>> {
    let raw: Vec<RawPane> =
        serde_json::from_str(json).map_err(|e| Error::parse(format!("wezterm cli list: {e}")))?;
    let mut windows: Vec<u64> = Vec::new();
    let mut tabs: Vec<(u64, u64)> = Vec::new();
    let mut panes = Vec::new();
    for p in raw {
        if !windows.contains(&p.window_id) {
            windows.push(p.window_id);
        }
        if !tabs.contains(&(p.window_id, p.tab_id)) {
            tabs.push((p.window_id, p.tab_id));
        }
        let window = windows.iter().position(|w| *w == p.window_id).unwrap_or(0) as u32 + 1;
        let index = tabs
            .iter()
            .filter(|(w, _)| *w == p.window_id)
            .position(|(_, t)| *t == p.tab_id)
            .unwrap_or(0) as u32
            + 1;
        panes.push(Pane {
            window,
            index,
            pane_id: p.pane_id,
            title: p.title,
            cwd: p
                .cwd
                .as_deref()
                .map(strip_file_url)
                .filter(|c| !c.is_empty()),
            tty: p
                .tty_name
                .map(|t| t.strip_prefix("/dev/").unwrap_or(&t).to_string())
                .filter(|t| !t.is_empty()),
        });
    }
    Ok(panes)
}

impl WezTerm {
    /// `None` when WezTerm is not running.
    fn panes(&self, ports: &Ports) -> Result<Option<Vec<Pane>>> {
        if ports.cmd.run("pgrep", &["-x", "wezterm-gui"]).is_err() {
            return Ok(None);
        }
        let out = ports
            .cmd
            .run("wezterm", &["cli", "list", "--format", "json"])?;
        parse_panes(&out).map(Some)
    }

    fn focus_pane(&self, ports: &Ports, pane_id: u64) -> Result<()> {
        let id = pane_id.to_string();
        ports
            .cmd
            .run("wezterm", &["cli", "activate-pane", "--pane-id", &id])?;
        ports.osa.run_jxa(r#"Application("WezTerm").activate()"#)?;
        Ok(())
    }
}

impl TerminalBackend for WezTerm {
    fn id(&self) -> &'static str {
        "wezterm"
    }

    fn list(&self, ports: &Ports) -> Result<Option<Vec<Tab>>> {
        let Some(panes) = self.panes(ports)? else {
            return Ok(None);
        };
        let mut tabs = Vec::new();
        for p in panes {
            tabs.push(Tab {
                terminal: "wezterm".into(),
                ref_: Some(format!("{REF_PREFIX}{}", p.pane_id)),
                title: p.title,
                cwd: p.cwd,
                tty: p.tty,
                window: p.window,
                index: p.index,
                agent: None,
            });
        }
        Ok(Some(tabs))
    }

    fn focus(&self, ports: &Ports, tab_ref: &str) -> Result<()> {
        let pane_id = tab_ref
            .strip_prefix(REF_PREFIX)
            .and_then(|n| n.parse::<u64>().ok())
            .ok_or_else(|| Error::other(format!("wezterm: bad tab ref {tab_ref}")))?;
        self.focus_pane(ports, pane_id)
    }

    fn focus_tty(&self, ports: &Ports, tty: &str) -> Result<bool> {
        let Some(panes) = self.panes(ports)? else {
            return Ok(false);
        };
        match panes.iter().find(|p| p.tty.as_deref() == Some(tty)) {
            Some(p) => self.focus_pane(ports, p.pane_id).map(|_| true),
            None => Ok(false),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tabs::{CommandRunner, OsaRunner, Sleeper, TtyWriter};
    use std::cell::RefCell;

    struct Cmd {
        running: bool,
        list: String,
        calls: RefCell<Vec<String>>,
    }

    impl CommandRunner for Cmd {
        fn run(&self, program: &str, args: &[&str]) -> Result<String> {
            self.calls
                .borrow_mut()
                .push(format!("{program} {}", args.join(" ")));
            match program {
                "pgrep" if !self.running => Err(Error::other("exit 1")),
                "wezterm" if args[1] == "list" => Ok(self.list.clone()),
                _ => Ok(String::new()),
            }
        }
    }

    struct Osa(RefCell<Vec<String>>);
    impl OsaRunner for Osa {
        fn run_jxa(&self, script: &str) -> Result<String> {
            self.0.borrow_mut().push(script.into());
            Ok(String::new())
        }
    }
    struct NoTty;
    impl TtyWriter for NoTty {
        fn write(&self, _: &str, _: &[u8]) -> Result<()> {
            Ok(())
        }
    }
    struct NoSleep;
    impl Sleeper for NoSleep {
        fn sleep_ms(&self, _: u64) {}
    }

    struct FailOn(&'static str, Cmd);
    impl CommandRunner for FailOn {
        fn run(&self, program: &str, args: &[&str]) -> Result<String> {
            if format!("{program} {}", args.join(" ")).contains(self.0) {
                return Err(Error::other("injected"));
            }
            self.1.run(program, args)
        }
    }
    struct FailOsa;
    impl OsaRunner for FailOsa {
        fn run_jxa(&self, _: &str) -> Result<String> {
            Err(Error::other("osa"))
        }
    }

    fn cmd_with(list: &str) -> Cmd {
        Cmd {
            running: true,

            list: list.into(),
            calls: RefCell::new(vec![]),
        }
    }

    fn errs(fail: &'static str, f: impl Fn(&Ports) -> Result<bool>) -> bool {
        let cmd = FailOn(fail, cmd_with(FIXTURE));
        let osa = Osa(RefCell::new(vec![]));
        let ports = Ports {
            osa: &osa,
            cmd: &cmd,
            tty: &NoTty,
            sleep: &NoSleep,
        };
        NoTty.write("x", b"").unwrap();
        NoSleep.sleep_ms(0);
        f(&ports).is_err()
    }

    const FIXTURE: &str = r#"[
      {"window_id":0,"tab_id":0,"pane_id":0,"workspace":"default","title":"zsh",
       "cwd":"file://mac.local/Users/x/proj","tty_name":"/dev/ttys003"},
      {"window_id":0,"tab_id":0,"pane_id":1,"workspace":"default","title":"split",
       "cwd":"file://mac.local/Users/x/My%20Caf%C3%A9%zz","tty_name":"/dev/ttys004"},
      {"window_id":0,"tab_id":3,"pane_id":5,"workspace":"default","title":"vim",
       "cwd":"/plain/path","tty_name":"ttys005"},
      {"window_id":7,"tab_id":9,"pane_id":8,"workspace":"w","title":"other",
       "cwd":"file://host","tty_name":""},
      {"window_id":7,"tab_id":10,"pane_id":9,"workspace":"w","title":"t2"}
    ]"#;

    fn run<R>(
        running: bool,
        list: &str,
        f: impl FnOnce(&Ports) -> R,
    ) -> (R, Vec<String>, Vec<String>) {
        let cmd = Cmd {
            running,
            list: list.into(),
            calls: RefCell::new(vec![]),
        };
        let osa = Osa(RefCell::new(vec![]));
        let ports = Ports {
            osa: &osa,
            cmd: &cmd,
            tty: &NoTty,
            sleep: &NoSleep,
        };
        let r = f(&ports);
        (r, cmd.calls.into_inner(), osa.0.into_inner())
    }

    #[test]
    fn list_parses_fixture() {
        let (r, calls, _) = run(true, FIXTURE, |p| WezTerm.list(p));
        let tabs = r.unwrap().unwrap();
        assert_eq!(calls[0], "pgrep -x wezterm-gui");
        assert_eq!(calls[1], "wezterm cli list --format json");
        assert_eq!(tabs.len(), 5);
        assert_eq!(WezTerm.id(), "wezterm");
        let t = &tabs[0];
        assert_eq!(t.terminal, "wezterm");
        assert_eq!(t.ref_.as_deref(), Some("wezterm:0"));
        assert_eq!(t.title, "zsh");
        assert_eq!(t.cwd.as_deref(), Some("/Users/x/proj"));
        assert_eq!(t.tty.as_deref(), Some("ttys003"));
        assert_eq!((t.window, t.index), (1, 1));
        assert_eq!(tabs[1].ref_.as_deref(), Some("wezterm:1"));
        assert_eq!(tabs[1].cwd.as_deref(), Some("/Users/x/My Café%zz"));
        assert_eq!(tabs[1].tty.as_deref(), Some("ttys004"));
        assert_eq!((tabs[1].window, tabs[1].index), (1, 1));
        assert_eq!(tabs[2].cwd.as_deref(), Some("/plain/path"));
        assert_eq!(tabs[2].tty.as_deref(), Some("ttys005"));
        assert_eq!((tabs[2].window, tabs[2].index), (1, 2));
        assert_eq!((tabs[3].window, tabs[3].index), (2, 1));
        assert_eq!((tabs[3].cwd.clone(), tabs[3].tty.clone()), (None, None));
        assert_eq!((tabs[4].window, tabs[4].index), (2, 2));
    }

    #[test]
    fn list_bad_json_is_error() {
        let (r, _, _) = run(true, "nope", |p| WezTerm.list(p));
        assert!(r.unwrap_err().to_string().contains("wezterm cli list"));
    }

    #[test]
    fn not_running_is_none() {
        let (r, calls, _) = run(false, FIXTURE, |p| WezTerm.list(p));
        assert!(r.unwrap().is_none());
        assert_eq!(calls.len(), 1);
        let (r, _, _) = run(false, FIXTURE, |p| WezTerm.focus_tty(p, "ttys003"));
        assert!(!r.unwrap());
    }

    #[test]
    fn focus_runs_cli_then_activates() {
        let (r, calls, osa) = run(true, FIXTURE, |p| WezTerm.focus(p, "wezterm:5"));
        r.unwrap();
        assert_eq!(calls, vec!["wezterm cli activate-pane --pane-id 5"]);
        assert_eq!(osa, vec![r#"Application("WezTerm").activate()"#]);
    }

    #[test]
    fn focus_rejects_bad_ref() {
        for bad in ["kitty:1", "wezterm:x", "wezterm:"] {
            let (r, calls, _) = run(true, FIXTURE, |p| WezTerm.focus(p, bad));
            assert!(r.unwrap_err().to_string().contains("bad tab ref"));
            assert!(calls.is_empty());
        }
    }

    #[test]
    fn focus_tty_matches() {
        let (r, calls, osa) = run(true, FIXTURE, |p| WezTerm.focus_tty(p, "ttys004"));
        assert!(r.unwrap());
        assert_eq!(
            calls.last().unwrap(),
            "wezterm cli activate-pane --pane-id 1"
        );
        assert_eq!(osa.len(), 1);
        let (r, calls, osa) = run(true, FIXTURE, |p| WezTerm.focus_tty(p, "ttys999"));
        assert!(!r.unwrap());
        assert_eq!(calls.len(), 2);
        assert!(osa.is_empty());
    }

    #[test]
    fn command_failures_propagate() {
        let w = WezTerm;
        assert!(errs("cli list", |p| w.list(p).map(|_| true)));
        assert!(errs("cli list", |p| w.focus_tty(p, "ttys003")));
        assert!(errs("activate-pane", |p| w
            .focus(p, "wezterm:1")
            .map(|_| true)));
        assert!(errs("activate-pane", |p| w.focus_tty(p, "ttys003")));
        let osa = FailOsa;
        let cmd = cmd_with(FIXTURE);
        let ports = Ports {
            osa: &osa,
            cmd: &cmd,
            tty: &NoTty,
            sleep: &NoSleep,
        };
        assert!(w.focus(&ports, "wezterm:1").is_err());
    }
}
