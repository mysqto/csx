//! iTerm2 terminal backend, driven through JXA.
//!
//! Built from Apple and iTerm2 scripting documentation only: iTerm2 is not
//! installed on the development machine, so the script shape has not been
//! checked against a live app.

use super::{Ports, Tab, TerminalBackend};
use crate::error::{Error, Result};
use serde::Deserialize;

const REF_PREFIX: &str = "iterm2:";

/// The iTerm2 backend.
pub struct ITerm2;

#[derive(Deserialize)]
struct RawWindow {
    tabs: Vec<RawTab>,
}

#[derive(Deserialize)]
struct RawTab {
    sessions: Vec<RawSession>,
}

#[derive(Deserialize)]
struct RawSession {
    id: String,
    name: String,
    tty: String,
    path: Option<String>,
}

const LIST_SCRIPT: &str = r#"(() => {
  let app;
  try {
    app = Application("iTerm2");
    if (!app.running()) { return "null"; }
  } catch (e) { return "null"; }
  const out = app.windows().map(w => ({
    tabs: w.tabs().map(t => ({
      sessions: t.sessions().map(s => {
        let path = null;
        try { path = s.variable({named: "session.path"}); } catch (e) {}
        return {id: s.id(), name: s.name(), tty: s.tty(), path: path || null};
      })
    }))
  }));
  return JSON.stringify(out);
})()"#;

fn focus_script(session_id: &str) -> Result<String> {
    let id = serde_json::to_string(session_id)?;
    Ok(format!(
        r#"(() => {{
  const wanted = {id};
  let app;
  try {{
    app = Application("iTerm2");
    if (!app.running()) {{ return "false"; }}
  }} catch (e) {{ return "false"; }}
  for (const w of app.windows()) {{
    for (const t of w.tabs()) {{
      for (const s of t.sessions()) {{
        if (s.id() === wanted) {{
          s.select();
          t.select();
          w.select();
          app.activate();
          return "true";
        }}
      }}
    }}
  }}
  return "false";
}})()"#
    ))
}

fn strip_dev(tty: &str) -> Option<String> {
    let t = tty.strip_prefix("/dev/").unwrap_or(tty);
    (!t.is_empty()).then(|| t.to_string())
}

impl TerminalBackend for ITerm2 {
    fn id(&self) -> &'static str {
        "iterm2"
    }

    fn list(&self, ports: &Ports) -> Result<Option<Vec<Tab>>> {
        let out = ports.osa.run_jxa(LIST_SCRIPT)?;
        let windows: Option<Vec<RawWindow>> = serde_json::from_str(out.trim())?;
        Ok(windows.map(|ws| {
            let mut tabs = Vec::new();
            for (wi, w) in ws.into_iter().enumerate() {
                for (ti, t) in w.tabs.into_iter().enumerate() {
                    for s in t.sessions {
                        tabs.push(Tab {
                            terminal: "iterm2".into(),
                            ref_: Some(format!("{REF_PREFIX}{}", s.id)),
                            title: s.name,
                            cwd: s.path.filter(|p| !p.is_empty()),
                            tty: strip_dev(&s.tty),
                            window: wi as u32 + 1,
                            index: ti as u32 + 1,
                            agent: None,
                        });
                    }
                }
            }
            tabs
        }))
    }

    fn focus(&self, ports: &Ports, tab_ref: &str) -> Result<()> {
        let id = tab_ref
            .strip_prefix(REF_PREFIX)
            .ok_or_else(|| Error::other(format!("not an iterm2 ref: {tab_ref}")))?;
        let out = ports.osa.run_jxa(&focus_script(id)?)?;
        if serde_json::from_str::<bool>(out.trim())? {
            Ok(())
        } else {
            Err(Error::other(format!("iterm2 session not found: {id}")))
        }
    }

    fn focus_tty(&self, ports: &Ports, tty: &str) -> Result<bool> {
        let tabs = self.list(ports)?.unwrap_or_default();
        match tabs.iter().find(|t| t.tty.as_deref() == Some(tty)) {
            Some(t) => {
                self.focus(ports, t.ref_.as_deref().unwrap_or_default())?;
                Ok(true)
            }
            None => Ok(false),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tabs::{CommandRunner, OsaRunner, Sleeper, TtyWriter};
    use std::cell::RefCell;

    struct Osa {
        replies: RefCell<Vec<String>>,
        scripts: RefCell<Vec<String>>,
    }
    impl Osa {
        fn new(replies: &[&str]) -> Self {
            Osa {
                replies: RefCell::new(replies.iter().rev().map(|s| s.to_string()).collect()),
                scripts: RefCell::new(vec![]),
            }
        }
    }
    impl OsaRunner for Osa {
        fn run_jxa(&self, script: &str) -> Result<String> {
            self.scripts.borrow_mut().push(script.to_string());
            Ok(self.replies.borrow_mut().pop().unwrap())
        }
    }
    struct Nop;
    impl CommandRunner for Nop {
        fn run(&self, _: &str, _: &[&str]) -> Result<String> {
            Ok(String::new())
        }
    }
    impl TtyWriter for Nop {
        fn write(&self, _: &str, _: &[u8]) -> Result<()> {
            Ok(())
        }
    }
    impl Sleeper for Nop {
        fn sleep_ms(&self, _: u64) {}
    }

    fn ports(osa: &Osa) -> Ports<'_> {
        Ports {
            osa,
            cmd: &Nop,
            tty: &Nop,
            sleep: &Nop,
        }
    }

    fn assert_script_shape(s: &str) {
        assert!(!s.contains("console.log"));
        assert!(s.trim_end().ends_with("})()"));
        assert!(s.contains("catch (e) { return"));
    }

    #[test]
    fn scripts_return_on_stdout_and_guard_missing_app() {
        assert_script_shape(LIST_SCRIPT);
        assert_script_shape(&focus_script("x").unwrap());
    }

    const LISTING: &str = r#"[
      {"tabs":[
        {"sessions":[
          {"id":"S-1","name":"vim","tty":"/dev/ttys001","path":"/work/a"},
          {"id":"S-2","name":"sh","tty":"/dev/ttys002","path":null}]},
        {"sessions":[{"id":"S-3","name":"x","tty":"","path":""}]}]},
      {"tabs":[{"sessions":[{"id":"S-4","name":"y","tty":"ttys004","path":"/b"}]}]}
    ]"#;

    #[test]
    fn list_parses() {
        let osa = Osa::new(&[LISTING]);
        let tabs = ITerm2.list(&ports(&osa)).unwrap().unwrap();
        assert_eq!(ITerm2.id(), "iterm2");
        assert_eq!(tabs.len(), 4);
        assert_eq!(tabs[0].ref_.as_deref(), Some("iterm2:S-1"));
        assert_eq!(tabs[0].tty.as_deref(), Some("ttys001"));
        assert_eq!(tabs[0].cwd.as_deref(), Some("/work/a"));
        assert_eq!((tabs[0].window, tabs[0].index), (1, 1));
        assert_eq!(tabs[1].cwd, None);
        assert_eq!((tabs[2].window, tabs[2].index), (1, 2));
        assert_eq!((tabs[2].tty.clone(), tabs[2].cwd.clone()), (None, None));
        assert_eq!(tabs[3].tty.as_deref(), Some("ttys004"));
        assert_eq!((tabs[3].window, tabs[3].index), (2, 1));
        assert_eq!(tabs[3].terminal, "iterm2");
        assert!(osa.scripts.borrow()[0].contains("running()"));
    }

    #[test]
    fn unused_ports_are_inert() {
        let osa = Osa::new(&[]);
        let p = ports(&osa);
        assert_eq!(p.cmd.run("x", &[]).unwrap(), "");
        p.tty.write("ttys001", b"").unwrap();
        p.sleep.sleep_ms(0);
    }

    #[test]
    fn list_not_running_is_none() {
        let osa = Osa::new(&["null\n"]);
        assert!(ITerm2.list(&ports(&osa)).unwrap().is_none());
    }

    #[test]
    fn focus_ref_script_selects_and_activates() {
        let osa = Osa::new(&["true\n"]);
        ITerm2.focus(&ports(&osa), "iterm2:S\"1").unwrap();
        let s = osa.scripts.borrow()[0].clone();
        assert_script_shape(&s);
        assert!(s.contains(r#"const wanted = "S\"1";"#));
        for call in ["s.select()", "t.select()", "w.select()", "app.activate()"] {
            assert!(s.contains(call), "{call}");
        }
    }

    #[test]
    fn focus_rejects_foreign_ref_and_missing_session() {
        let osa = Osa::new(&["false"]);
        assert!(ITerm2.focus(&ports(&osa), "tmux:1").is_err());
        assert!(osa.scripts.borrow().is_empty());
        let e = ITerm2.focus(&ports(&osa), "iterm2:S-9").unwrap_err();
        assert!(e.to_string().contains("S-9"));
    }

    #[test]
    fn focus_tty_matches_listed_tty() {
        let osa = Osa::new(&[LISTING, "true"]);
        assert!(ITerm2.focus_tty(&ports(&osa), "ttys002").unwrap());
        assert!(osa.scripts.borrow()[1].contains(r#""S-2""#));
    }

    #[test]
    fn focus_tty_unknown_is_false() {
        let osa = Osa::new(&[LISTING, "null"]);
        assert!(!ITerm2.focus_tty(&ports(&osa), "ttys099").unwrap());
        // App not running: nothing to focus.
        assert!(!ITerm2.focus_tty(&ports(&osa), "ttys001").unwrap());
    }

    #[test]
    fn malformed_json_is_error() {
        let osa = Osa::new(&["not json", "oops"]);
        assert!(ITerm2.list(&ports(&osa)).is_err());
        assert!(ITerm2.focus(&ports(&osa), "iterm2:S-1").is_err());
    }
}
