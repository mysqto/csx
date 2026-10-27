//! kitty terminal backend, built from documentation: `kitten @ ls` prints
//! os_windows, tabs and windows as JSON and `kitten @ focus-window` focuses one.

use super::process::ProcessTable;
use super::{Ports, Tab, TerminalBackend};
use crate::error::{Error, Result};
use serde::Deserialize;

const REF_PREFIX: &str = "kitty:";
const REMOTE_CONTROL_HINT: &str = "kitty: turn on allow_remote_control and listen_on in kitty.conf";

/// The kitty backend.
pub struct Kitty {
    /// Remote-control socket, from `KITTY_LISTEN_ON` or `--kitty-socket`.
    pub socket: Option<String>,
}

#[derive(Deserialize)]
struct Fg {
    pid: u32,
}

#[derive(Deserialize)]
struct RawWindow {
    id: u64,
    #[serde(default)]
    title: String,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    pid: Option<u32>,
    #[serde(default)]
    foreground_processes: Vec<Fg>,
}

#[derive(Deserialize)]
struct RawTab {
    #[serde(default)]
    windows: Vec<RawWindow>,
}

#[derive(Deserialize)]
struct RawOsWindow {
    #[serde(default)]
    tabs: Vec<RawTab>,
}

/// One row per kitty window (split), with the kitty window id.
type Entry = (u64, Tab);

fn parse_entries(json: &str, procs: &ProcessTable) -> Result<Vec<Entry>> {
    let os: Vec<RawOsWindow> =
        serde_json::from_str(json).map_err(|e| Error::parse(format!("kitten @ ls: {e}")))?;
    let mut entries = Vec::new();
    for (wi, w) in os.iter().enumerate() {
        for (ti, t) in w.tabs.iter().enumerate() {
            for win in &t.windows {
                let tty = win
                    .foreground_processes
                    .first()
                    .and_then(|f| procs.get(f.pid))
                    .and_then(|p| p.tty.clone())
                    .or_else(|| win.pid.and_then(|pid| procs.get(pid)?.tty.clone()));
                entries.push((
                    win.id,
                    Tab {
                        terminal: "kitty".into(),
                        ref_: Some(format!("{REF_PREFIX}{}", win.id)),
                        title: win.title.clone(),
                        cwd: win.cwd.clone().filter(|c| !c.is_empty()),
                        tty,
                        window: wi as u32 + 1,
                        index: ti as u32 + 1,
                        agent: None,
                    },
                ));
            }
        }
    }
    Ok(entries)
}

/// kitty's `listen_on unix:/tmp/kitty` really listens on `/tmp/kitty-<pid>`.
fn pid_socket(pid: u32) -> String {
    format!("unix:/tmp/kitty-{pid}")
}

fn kitten(ports: &Ports, socket: Option<&str>, sub: &[&str]) -> Result<String> {
    let mut args: Vec<&str> = vec!["@"];
    if let Some(s) = socket {
        args.extend(["--to", s]);
    }
    args.extend(sub);
    ports.cmd.run("kitten", &args)
}

impl Kitty {
    /// Pids of running kitty processes; `None` when kitty is not running.
    fn pids(ports: &Ports) -> Option<Vec<u32>> {
        let out = ports.cmd.run("pgrep", &["-x", "kitty"]).ok()?;
        Some(
            out.lines()
                .filter_map(|l| l.trim().parse::<u32>().ok())
                .collect(),
        )
    }

    /// Sockets to try, in order: the explicit one, else one per kitty pid,
    /// else none (kitten's own default).
    fn candidates(&self, pids: &[u32]) -> Vec<Option<String>> {
        match (&self.socket, pids) {
            (Some(s), _) => vec![Some(s.clone())],
            (None, []) => vec![None],
            (None, pids) => pids.iter().map(|p| Some(pid_socket(*p))).collect(),
        }
    }

    /// Run `f` on each candidate socket; the first success wins and comes
    /// back with its socket. When all fail, the last error.
    fn first_ok<T>(
        candidates: Vec<Option<String>>,
        f: impl Fn(Option<&str>) -> Result<T>,
    ) -> Result<(Option<String>, T)> {
        let mut last = Error::other("no kitty socket to try");
        for c in candidates {
            match f(c.as_deref()) {
                Ok(v) => return Ok((c, v)),
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    /// The socket that answered and the entries; `None` when kitty is not
    /// running.
    fn entries(&self, ports: &Ports) -> Result<Option<(Option<String>, Vec<Entry>)>> {
        let Some(pids) = Self::pids(ports) else {
            return Ok(None);
        };
        let (socket, out) =
            Self::first_ok(self.candidates(&pids), |s| kitten(ports, s, &["ls"]))
                .map_err(|e| Error::other(format!("{REMOTE_CONTROL_HINT} (kitten: {e})")))?;
        let procs = ProcessTable::load(ports.cmd)?;
        parse_entries(&out, &procs).map(|es| Some((socket, es)))
    }

    fn focus_window(ports: &Ports, socket: Option<&str>, id: u64) -> Result<()> {
        let m = format!("id:{id}");
        kitten(ports, socket, &["focus-window", "--match", &m])?;
        ports.osa.run_jxa(r#"Application("kitty").activate()"#)?;
        Ok(())
    }
}

impl TerminalBackend for Kitty {
    fn id(&self) -> &'static str {
        "kitty"
    }

    fn list(&self, ports: &Ports) -> Result<Option<Vec<Tab>>> {
        Ok(self
            .entries(ports)?
            .map(|(_, es)| es.into_iter().map(|(_, t)| t).collect()))
    }

    fn focus(&self, ports: &Ports, tab_ref: &str) -> Result<()> {
        let id = tab_ref
            .strip_prefix(REF_PREFIX)
            .and_then(|n| n.parse::<u64>().ok())
            .ok_or_else(|| Error::other(format!("kitty: bad tab ref {tab_ref}")))?;
        // An explicit socket needs no probing; otherwise find the pids.
        let pids = if self.socket.is_some() {
            Vec::new()
        } else {
            Self::pids(ports).unwrap_or_default()
        };
        let m = format!("id:{id}");
        Self::first_ok(self.candidates(&pids), |s| {
            kitten(ports, s, &["focus-window", "--match", &m])
        })?;
        ports.osa.run_jxa(r#"Application("kitty").activate()"#)?;
        Ok(())
    }

    fn focus_tty(&self, ports: &Ports, tty: &str) -> Result<bool> {
        let Some((socket, entries)) = self.entries(ports)? else {
            return Ok(false);
        };
        match entries.iter().find(|(_, t)| t.tty.as_deref() == Some(tty)) {
            Some((id, _)) => Self::focus_window(ports, socket.as_deref(), *id).map(|_| true),
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
        remote: bool,
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
                "kitten" if !self.remote => Err(Error::other("kitten failed: no socket")),
                "kitten" if args.contains(&"ls") => Ok(self.list.clone()),
                "ps" => Ok("100 1 100 100 ttys001 01:00 -zsh\n\
                            200 1 200 200 ttys002 01:00 /bin/vim\n\
                            97 1 97 97 ttys003 01:00 /bin/kitty\n"
                    .into()),
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
            remote: true,
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
      {"id":1,"tabs":[
        {"id":1,"title":"tab one","windows":[
          {"id":11,"title":"zsh","cwd":"/Users/x/proj","pid":99,
           "foreground_processes":[{"pid":100,"cmdline":["-zsh"],"cwd":"/Users/x/proj"}]},
          {"id":12,"title":"vim","cwd":"/Users/x/other","pid":98,
           "foreground_processes":[{"pid":200,"cmdline":["vim"],"cwd":"/Users/x/other"}]}
        ]},
        {"id":2,"title":"empty","windows":[]},
        {"id":3,"title":"t3","windows":[
          {"id":13,"title":"nofg","cwd":"","pid":97,"foreground_processes":[]}
        ]}
      ]},
      {"id":2,"tabs":[
        {"id":4,"title":"t4","windows":[
          {"id":21,"title":"ghost","cwd":"/g","pid":96,
           "foreground_processes":[{"pid":999}]}
        ]}
      ]},
      {"id":3}
    ]"#;

    fn run<R>(
        running: bool,
        remote: bool,
        socket: Option<&str>,
        f: impl FnOnce(&Kitty, &Ports) -> R,
    ) -> (R, Vec<String>, Vec<String>) {
        let cmd = Cmd {
            running,
            remote,
            list: FIXTURE.into(),
            calls: RefCell::new(vec![]),
        };
        let osa = Osa(RefCell::new(vec![]));
        let ports = Ports {
            osa: &osa,
            cmd: &cmd,
            tty: &NoTty,
            sleep: &NoSleep,
        };
        let k = Kitty {
            socket: socket.map(String::from),
        };
        let r = f(&k, &ports);
        (r, cmd.calls.into_inner(), osa.0.into_inner())
    }

    #[test]
    fn list_parses_fixture() {
        let (r, calls, _) = run(true, true, None, |k, p| k.list(p));
        let tabs = r.unwrap().unwrap();
        assert_eq!(calls[0], "pgrep -x kitty");
        assert_eq!(calls[1], "kitten @ ls");
        assert_eq!(
            calls[2],
            "ps -axo pid=,ppid=,pgid=,tpgid=,tty=,etime=,args="
        );
        assert_eq!(calls.len(), 3);
        assert_eq!(Kitty { socket: None }.id(), "kitty");
        assert_eq!(tabs.len(), 4);
        let t = &tabs[0];
        assert_eq!(t.terminal, "kitty");
        assert_eq!(t.ref_.as_deref(), Some("kitty:11"));
        assert_eq!(t.title, "zsh");
        assert_eq!(t.cwd.as_deref(), Some("/Users/x/proj"));
        assert_eq!(t.tty.as_deref(), Some("ttys001"));
        assert_eq!((t.window, t.index), (1, 1));
        assert_eq!(tabs[1].ref_.as_deref(), Some("kitty:12"));
        assert_eq!(tabs[1].tty.as_deref(), Some("ttys002"));
        assert_eq!((tabs[1].window, tabs[1].index), (1, 1));
        // No foreground process: falls back to the window pid.
        assert_eq!((tabs[2].window, tabs[2].index), (1, 3));
        assert_eq!(tabs[2].cwd, None);
        assert_eq!(tabs[2].tty.as_deref(), Some("ttys003"));
        assert_eq!((tabs[3].window, tabs[3].index), (2, 1));
        assert_eq!(tabs[3].tty, None);
    }

    #[test]
    fn list_bad_json_is_error() {
        let cmd = Cmd {
            running: true,
            remote: true,
            list: "nope".into(),
            calls: RefCell::new(vec![]),
        };
        let osa = Osa(RefCell::new(vec![]));
        let ports = Ports {
            osa: &osa,
            cmd: &cmd,
            tty: &NoTty,
            sleep: &NoSleep,
        };
        let e = Kitty { socket: None }.list(&ports).unwrap_err();
        assert!(e.to_string().contains("kitten @ ls"));
    }

    #[test]
    fn not_running_is_none() {
        let (r, calls, _) = run(false, true, None, |k, p| k.list(p));
        assert!(r.unwrap().is_none());
        assert_eq!(calls.len(), 1);
        let (r, _, _) = run(false, true, None, |k, p| k.focus_tty(p, "ttys001"));
        assert!(!r.unwrap());
    }

    #[test]
    fn remote_control_off_is_actionable_error() {
        let (r, _, _) = run(true, false, None, |k, p| k.list(p));
        assert_eq!(
            r.unwrap_err().to_string(),
            "kitty: turn on allow_remote_control and listen_on in kitty.conf (kitten: kitten failed: no socket)"
        );
    }

    #[test]
    fn socket_flag_is_passed() {
        let (r, calls, _) = run(true, true, Some("unix:/tmp/k"), |k, p| {
            k.focus(p, "kitty:12")
        });
        r.unwrap();
        assert_eq!(
            calls,
            vec!["kitten @ --to unix:/tmp/k focus-window --match id:12"]
        );
        let (_, calls, _) = run(true, true, Some("unix:/tmp/k"), |k, p| k.list(p));
        assert_eq!(calls[1], "kitten @ --to unix:/tmp/k ls");
    }

    #[test]
    fn focus_runs_cli_then_activates() {
        let (r, calls, osa) = run(true, true, None, |k, p| k.focus(p, "kitty:11"));
        r.unwrap();
        assert_eq!(
            calls,
            vec!["pgrep -x kitty", "kitten @ focus-window --match id:11"]
        );
        assert_eq!(osa, vec![r#"Application("kitty").activate()"#]);
    }

    #[test]
    fn focus_rejects_bad_ref() {
        for bad in ["wezterm:1", "kitty:x", "kitty:"] {
            let (r, calls, _) = run(true, true, None, |k, p| k.focus(p, bad));
            assert!(r.unwrap_err().to_string().contains("bad tab ref"));
            assert!(calls.is_empty());
        }
    }

    #[test]
    fn focus_tty_matches() {
        let (r, calls, osa) = run(true, true, None, |k, p| k.focus_tty(p, "ttys002"));
        assert!(r.unwrap());
        assert_eq!(calls.last().unwrap(), "kitten @ focus-window --match id:12");
        assert_eq!(osa.len(), 1);
        let (r, _, osa) = run(true, true, None, |k, p| k.focus_tty(p, "ttys999"));
        assert!(!r.unwrap());
        assert!(osa.is_empty());
    }

    #[test]
    fn command_failures_propagate() {
        let k = Kitty { socket: None };
        assert!(errs("ps ", |p| k.list(p).map(|_| true)));
        assert!(errs("ps ", |p| k.focus_tty(p, "ttys001")));
        assert!(errs("focus-window", |p| k
            .focus(p, "kitty:1")
            .map(|_| true)));
        assert!(errs("focus-window", |p| k.focus_tty(p, "ttys001")));
        let osa = FailOsa;
        let cmd = cmd_with(FIXTURE);
        let ports = Ports {
            osa: &osa,
            cmd: &cmd,
            tty: &NoTty,
            sleep: &NoSleep,
        };
        assert!(k.focus(&ports, "kitty:1").is_err());
    }

    /// A command fake where kitty runs with `pids` and only `answers` replies.
    struct Pids {
        pids: &'static str,
        answers: &'static str,
        calls: RefCell<Vec<String>>,
    }
    impl CommandRunner for Pids {
        fn run(&self, program: &str, args: &[&str]) -> Result<String> {
            let line = format!("{program} {}", args.join(" "));
            self.calls.borrow_mut().push(line.clone());
            match program {
                "pgrep" => Ok(self.pids.into()),
                "kitten" if line.contains(self.answers) => Ok(if line.contains(" ls") {
                    FIXTURE.into()
                } else {
                    String::new()
                }),
                "kitten" => Err(Error::other("no such socket")),
                _ => Ok("100 1 100 100 ttys001 01:00 -zsh\n\
                         200 1 200 200 ttys002 01:00 /bin/vim\n"
                    .into()),
            }
        }
    }

    fn with_pids<R>(
        pids: &'static str,
        answers: &'static str,
        f: impl FnOnce(&Ports) -> R,
    ) -> (R, Vec<String>) {
        let cmd = Pids {
            pids,
            answers,
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
        (r, cmd.calls.into_inner())
    }

    #[test]
    fn list_tries_each_pid_socket_until_one_answers() {
        let k = Kitty { socket: None };
        let (r, calls) = with_pids("31\n42\n", "/tmp/kitty-42", |p| k.list(p));
        assert_eq!(r.unwrap().unwrap().len(), 4);
        assert_eq!(
            &calls[..3],
            [
                "pgrep -x kitty",
                "kitten @ --to unix:/tmp/kitty-31 ls",
                "kitten @ --to unix:/tmp/kitty-42 ls"
            ]
        );
        // When no pid socket answers, the error carries the hint and the last error.
        let (r, _) = with_pids("31\n42\n", "nothing", |p| k.list(p));
        let e = r.unwrap_err().to_string();
        assert!(e.contains("allow_remote_control") && e.contains("no such socket"));
    }

    #[test]
    fn explicit_socket_beats_pid_sockets() {
        let k = Kitty {
            socket: Some("unix:/tmp/mine".into()),
        };
        let (r, calls) = with_pids("31\n", "/tmp/mine", |p| k.list(p));
        r.unwrap().unwrap();
        assert_eq!(calls[1], "kitten @ --to unix:/tmp/mine ls");
        assert!(!calls.iter().any(|c| c.contains("kitty-31")));
    }

    #[test]
    fn focus_tries_pid_sockets_and_reuses_the_answering_one_for_focus_tty() {
        let k = Kitty { socket: None };
        let (r, calls) = with_pids("31\n42\n", "/tmp/kitty-42", |p| k.focus(p, "kitty:12"));
        r.unwrap();
        assert_eq!(
            calls,
            [
                "pgrep -x kitty",
                "kitten @ --to unix:/tmp/kitty-31 focus-window --match id:12",
                "kitten @ --to unix:/tmp/kitty-42 focus-window --match id:12"
            ]
        );
        let (r, calls) = with_pids("31\n42\n", "/tmp/kitty-42", |p| k.focus_tty(p, "ttys002"));
        assert!(r.unwrap());
        assert_eq!(
            calls.last().unwrap(),
            "kitten @ --to unix:/tmp/kitty-42 focus-window --match id:12"
        );
        // Nothing answers: the focus error surfaces.
        let (r, _) = with_pids("31\n", "nothing", |p| k.focus(p, "kitty:12"));
        assert!(r.unwrap_err().to_string().contains("no such socket"));
    }

    #[test]
    fn explicit_socket_focus_skips_pgrep() {
        let k = Kitty {
            socket: Some("unix:/tmp/mine".into()),
        };
        let (r, calls) = with_pids("31\n", "/tmp/mine", |p| k.focus(p, "kitty:12"));
        r.unwrap();
        assert_eq!(
            calls,
            ["kitten @ --to unix:/tmp/mine focus-window --match id:12"]
        );
    }

    #[test]
    fn candidates_cover_every_branch() {
        let none = Kitty { socket: None };
        assert_eq!(none.candidates(&[]), [None]);
        assert_eq!(
            none.candidates(&[7]),
            [Some("unix:/tmp/kitty-7".to_string())]
        );
        let some = Kitty {
            socket: Some("s".into()),
        };
        assert_eq!(some.candidates(&[7]), [Some("s".to_string())]);
        // No candidate at all keeps the default error.
        let e = Kitty::first_ok::<()>(vec![], |_| Ok(())).unwrap_err();
        assert!(e.to_string().contains("no kitty socket"));
    }
}
