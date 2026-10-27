//! Terminal.app terminal backend, driven through JXA.
//!
//! Terminal.app has no per-tab working directory, so `cwd` is always `None`.
//! The script shape is checked live in Task 9.

use super::{Ports, Tab, TerminalBackend};
use crate::error::{Error, Result};
use serde::Deserialize;

const REF_PREFIX: &str = "terminal:";

/// The Terminal.app backend.
pub struct TerminalApp;

#[derive(Deserialize)]
struct RawWindow {
    id: u64,
    name: String,
    tabs: Vec<RawTab>,
}

#[derive(Deserialize)]
struct RawTab {
    tty: String,
    #[serde(rename = "customTitle")]
    custom_title: Option<String>,
}

const LIST_SCRIPT: &str = r#"
const app = Application("Terminal");
if (!app.running()) {
  console.log("null");
} else {
  const out = app.windows().map(w => ({
    id: w.id(),
    name: w.name(),
    tabs: w.tabs().map(t => ({tty: t.tty(), customTitle: t.customTitle() || null}))
  }));
  console.log(JSON.stringify(out));
}
"#;

fn focus_script(window_id: u64, index: u64) -> String {
    format!(
        r#"
const app = Application("Terminal");
let found = false;
if (app.running()) {{
  for (const w of app.windows()) {{
    if (!found && w.id() === {window_id}) {{
      const tabs = w.tabs();
      if ({index} >= 1 && {index} <= tabs.length) {{
        found = true;
        tabs[{index} - 1].selected = true;
        w.index = 1;
        app.activate();
      }}
    }}
  }}
}}
console.log(JSON.stringify(found));
"#
    )
}

fn parse_ref(tab_ref: &str) -> Option<(u64, u64)> {
    let (w, i) = tab_ref.strip_prefix(REF_PREFIX)?.split_once(':')?;
    Some((w.parse().ok()?, i.parse().ok()?))
}

impl TerminalBackend for TerminalApp {
    fn id(&self) -> &'static str {
        "terminal"
    }

    fn list(&self, ports: &Ports) -> Result<Option<Vec<Tab>>> {
        let out = ports.osa.run_jxa(LIST_SCRIPT)?;
        let windows: Option<Vec<RawWindow>> = serde_json::from_str(out.trim())?;
        Ok(windows.map(|ws| {
            let mut tabs = Vec::new();
            for (wi, w) in ws.into_iter().enumerate() {
                for (ti, t) in w.tabs.into_iter().enumerate() {
                    let tty = t.tty.strip_prefix("/dev/").unwrap_or(&t.tty);
                    tabs.push(Tab {
                        terminal: "terminal".into(),
                        ref_: Some(format!("{REF_PREFIX}{}:{}", w.id, ti + 1)),
                        title: t
                            .custom_title
                            .filter(|c| !c.is_empty())
                            .unwrap_or_else(|| w.name.clone()),
                        cwd: None,
                        tty: (!tty.is_empty()).then(|| tty.to_string()),
                        window: wi as u32 + 1,
                        index: ti as u32 + 1,
                        agent: None,
                    });
                }
            }
            tabs
        }))
    }

    fn focus(&self, ports: &Ports, tab_ref: &str) -> Result<()> {
        let (w, i) = parse_ref(tab_ref)
            .ok_or_else(|| Error::other(format!("not a terminal ref: {tab_ref}")))?;
        let out = ports.osa.run_jxa(&focus_script(w, i))?;
        if serde_json::from_str::<bool>(out.trim())? {
            Ok(())
        } else {
            Err(Error::other(format!("terminal tab not found: {tab_ref}")))
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

    const LISTING: &str = r#"[
      {"id":101,"name":"win one","tabs":[
        {"tty":"/dev/ttys001","customTitle":"build"},
        {"tty":"/dev/ttys002","customTitle":null},
        {"tty":"","customTitle":""}]},
      {"id":202,"name":"win two","tabs":[{"tty":"ttys004","customTitle":null}]}
    ]"#;

    #[test]
    fn list_parses() {
        let osa = Osa::new(&[LISTING]);
        let tabs = TerminalApp.list(&ports(&osa)).unwrap().unwrap();
        assert_eq!(TerminalApp.id(), "terminal");
        assert_eq!(tabs.len(), 4);
        assert_eq!(tabs[0].ref_.as_deref(), Some("terminal:101:1"));
        assert_eq!(tabs[0].title, "build");
        assert_eq!(tabs[0].tty.as_deref(), Some("ttys001"));
        assert_eq!(tabs[0].cwd, None);
        assert_eq!(tabs[1].title, "win one");
        assert_eq!((tabs[2].index, tabs[2].tty.clone()), (3, None));
        assert_eq!(tabs[2].title, "win one");
        assert_eq!(tabs[3].ref_.as_deref(), Some("terminal:202:1"));
        assert_eq!((tabs[3].window, tabs[3].index), (2, 1));
        assert_eq!(tabs[3].tty.as_deref(), Some("ttys004"));
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
        assert!(TerminalApp.list(&ports(&osa)).unwrap().is_none());
    }

    #[test]
    fn focus_ref_script_selects_and_activates() {
        let osa = Osa::new(&["true\n"]);
        TerminalApp.focus(&ports(&osa), "terminal:101:2").unwrap();
        let s = osa.scripts.borrow()[0].clone();
        assert!(s.contains("w.id() === 101"));
        assert!(s.contains("tabs[2 - 1].selected = true"));
        assert!(s.contains("w.index = 1"));
        assert!(s.contains("app.activate()"));
    }

    #[test]
    fn focus_rejects_bad_ref_and_missing_tab() {
        let osa = Osa::new(&["false"]);
        for bad in ["iterm2:1", "terminal:1", "terminal:x:1", "terminal:1:y"] {
            assert!(TerminalApp.focus(&ports(&osa), bad).is_err(), "{bad}");
        }
        assert!(osa.scripts.borrow().is_empty());
        let e = TerminalApp.focus(&ports(&osa), "terminal:9:9").unwrap_err();
        assert!(e.to_string().contains("terminal:9:9"));
    }

    #[test]
    fn focus_tty_matches_listed_tty() {
        let osa = Osa::new(&[LISTING, "true"]);
        assert!(TerminalApp.focus_tty(&ports(&osa), "ttys004").unwrap());
        assert!(osa.scripts.borrow()[1].contains("w.id() === 202"));
    }

    #[test]
    fn focus_tty_unknown_is_false() {
        let osa = Osa::new(&[LISTING, "null"]);
        assert!(!TerminalApp.focus_tty(&ports(&osa), "ttys099").unwrap());
        // App not running: nothing to focus.
        assert!(!TerminalApp.focus_tty(&ports(&osa), "ttys001").unwrap());
    }

    #[test]
    fn malformed_json_is_error() {
        let osa = Osa::new(&["not json", "oops"]);
        assert!(TerminalApp.list(&ports(&osa)).is_err());
        assert!(TerminalApp.focus(&ports(&osa), "terminal:1:1").is_err());
    }
}
