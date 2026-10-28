//! Ghostty terminal backend: lists terminals and focuses one by id, or by tty
//! through a temporary title marker.

use super::{app_running, launch, write_tty, Ports, Tab, TerminalBackend, GHOSTTY};
use crate::error::{Error, Result};
use serde::Deserialize;
use std::collections::HashMap;

/// How many times the title marker is written before giving up.
pub const MARKER_ATTEMPTS: u32 = 6;

/// The Ghostty backend.
pub struct Ghostty;

#[derive(Deserialize)]
struct Entry {
    w: u32,
    t: u32,
    id: String,
    name: String,
    cwd: Option<String>,
}

/// An OSC 2 sequence that sets the window title: `ESC ] 2 ; <title> BEL`.
/// Control characters are removed from `title`.
pub fn osc_title(title: &str) -> Vec<u8> {
    let clean: String = title.chars().filter(|c| !c.is_control()).collect();
    let mut out = b"\x1b]2;".to_vec();
    out.extend_from_slice(clean.as_bytes());
    out.push(0x07);
    out
}

/// Embed `s` in JXA source as a string literal.
fn lit(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_default()
}

const LIST_JXA: &str = r#"(() => {
  let g;
  try {
    g = Application("Ghostty");
    if (!g.running()) return "null";
  } catch (e) {
    return "null";
  }
  // Three Apple events return nested arrays [window][tab][terminal]. They are
  // not atomic, so a terminal that opens or closes in between must not throw.
  const ids = g.windows.tabs.terminals.id();
  const names = g.windows.tabs.terminals.name();
  const cwds = g.windows.tabs.terminals.workingDirectory();
  const out = [];
  ids.forEach((tabs, wi) => {
    tabs.forEach((terms, ti) => {
      terms.forEach((id, k) => {
        out.push({ w: wi + 1, t: ti + 1, id: id, name: names[wi]?.[ti]?.[k] ?? "", cwd: cwds[wi]?.[ti]?.[k] ?? "" });
      });
    });
  });
  return JSON.stringify(out);
})()"#;

fn find_jxa(marker: &str) -> String {
    format!(
        r#"(() => {{
  const ids = Application("Ghostty").terminals.whose({{ name: {} }}).id();
  return ids.length ? String(ids[0]) : "";
}})()"#,
        lit(marker)
    )
}

/// Focuses the terminal and prints its name as JSON, or `null` when missing.
fn focus_jxa(id: &str) -> String {
    format!(
        r#"(() => {{
  const g = Application("Ghostty");
  const ids = g.terminals.id();
  if (ids.indexOf({}) < 0) return "null";
  const terminal = g.terminals.byId({});
  g.focus(terminal);
  g.activate();
  return JSON.stringify(terminal.name());
}})()"#,
        lit(id),
        lit(id)
    )
}

/// Opens a tab in the front window, or a window when there is none, that
/// starts in `cwd` and types `line` and a return. The names come from
/// Ghostty's scripting dictionary: `new surface configuration` with
/// `initial working directory` and `initial input`, `new tab` with `in` and
/// `with configuration`, `new window`, `front window`.
fn open_jxa(cwd: &str, line: &str) -> String {
    format!(
        r#"(() => {{
  const g = Application("Ghostty");
  const c = g.newSurfaceConfiguration();
  c.initialWorkingDirectory = {};
  c.initialInput = {} + "\r";
  if (g.windows().length > 0) {{
    g.newTab({{in: g.frontWindow(), withConfiguration: c}});
  }} else {{
    g.newWindow({{withConfiguration: c}});
  }}
  g.activate();
  return "ok";
}})()"#,
        lit(cwd),
        lit(line)
    )
}

fn entries(ports: &Ports) -> Result<Option<Vec<Entry>>> {
    let out = ports.osa.run_jxa(LIST_JXA)?;
    Ok(serde_json::from_str(out.trim())?)
}

/// Focus the terminal `id`; returns its name afterwards.
fn focus_id(ports: &Ports, id: &str) -> Result<String> {
    let out = ports.osa.run_jxa(&focus_jxa(id))?;
    let name: Option<String> = serde_json::from_str(out.trim())?;
    name.ok_or_else(|| Error::other("Ghostty tab not found"))
}

impl TerminalBackend for Ghostty {
    fn id(&self) -> &'static str {
        "ghostty"
    }

    fn list(&self, ports: &Ports) -> Result<Option<Vec<Tab>>> {
        Ok(entries(ports)?.map(|es| {
            es.into_iter()
                .map(|e| Tab {
                    terminal: "ghostty".into(),
                    ref_: Some(format!("ghostty:{}", e.id)),
                    title: e.name,
                    cwd: e.cwd.filter(|c| !c.is_empty()),
                    window: e.w,
                    index: e.t,
                    ..Tab::default()
                })
                .collect()
        }))
    }

    fn focus(&self, ports: &Ports, tab_ref: &str) -> Result<()> {
        let id = tab_ref
            .strip_prefix("ghostty:")
            .ok_or_else(|| Error::other(format!("not a Ghostty ref: {tab_ref}")))?;
        focus_id(ports, id).map(|_| ())
    }

    fn focus_tty(&self, ports: &Ports, tty: &str) -> Result<bool> {
        let Some(snapshot) = entries(ports)? else {
            return Ok(false);
        };
        let titles: HashMap<String, String> =
            snapshot.into_iter().map(|e| (e.id, e.name)).collect();
        for n in 1..=MARKER_ATTEMPTS {
            let marker = format!("csx-{tty}-{n}");
            write_tty(ports, tty, &osc_title(&marker))?;
            ports.sleep.sleep_ms(50 * u64::from(n));
            let id = ports.osa.run_jxa(&find_jxa(&marker))?.trim().to_string();
            if id.is_empty() {
                continue;
            }
            let name = focus_id(ports, &id)?;
            if name == marker {
                if let Some(old) = titles.get(&id) {
                    write_tty(ports, tty, &osc_title(old))?;
                }
            }
            return Ok(true);
        }
        Ok(false)
    }

    fn open_tab(&self, ports: &Ports, cwd: &str, line: &str) -> Result<()> {
        let running = |p: &Ports| app_running(p, "Ghostty");
        if !running(ports) {
            launch(ports, &GHOSTTY, &running)?;
        }
        ports.osa.run_jxa(&open_jxa(cwd, line))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tabs::test_support::{log, FakeOs};
    use crate::tabs::{CommandRunner, OsaRunner, Sleeper, TtyWriter};
    use std::cell::RefCell;
    use std::collections::VecDeque;

    #[derive(Default)]
    struct Osa {
        responses: RefCell<VecDeque<Result<String>>>,
        scripts: RefCell<Vec<String>>,
    }
    impl Osa {
        fn with(r: &[&str]) -> Self {
            let o = Osa::default();
            for x in r {
                o.responses.borrow_mut().push_back(Ok((*x).to_string()));
            }
            o
        }
    }
    impl OsaRunner for Osa {
        fn run_jxa(&self, script: &str) -> Result<String> {
            self.scripts.borrow_mut().push(script.to_string());
            self.responses.borrow_mut().pop_front().expect("response")
        }
    }
    #[derive(Default)]
    struct Tty(RefCell<Vec<(String, Vec<u8>)>>);
    impl TtyWriter for Tty {
        fn write(&self, tty: &str, bytes: &[u8]) -> Result<()> {
            self.0.borrow_mut().push((tty.into(), bytes.to_vec()));
            Ok(())
        }
    }
    #[derive(Default)]
    struct Sleep(RefCell<Vec<u64>>);
    impl Sleeper for Sleep {
        fn sleep_ms(&self, ms: u64) {
            self.0.borrow_mut().push(ms);
        }
    }
    struct NoCmd;
    impl CommandRunner for NoCmd {
        fn run(&self, _: &str, _: &[&str]) -> Result<String> {
            Ok(String::new())
        }
    }

    struct Rig {
        osa: Osa,
        tty: Tty,
        sleep: Sleep,
    }
    impl Rig {
        fn new(r: &[&str]) -> Self {
            Rig {
                osa: Osa::with(r),
                tty: Tty::default(),
                sleep: Sleep::default(),
            }
        }
        fn ports(&self) -> Ports<'_> {
            Ports {
                osa: &self.osa,
                cmd: &NoCmd,
                tty: &self.tty,
                sleep: &self.sleep,
            }
        }
        fn writes(&self) -> Vec<Vec<u8>> {
            self.tty.0.borrow().iter().map(|w| w.1.clone()).collect()
        }
    }

    const SNAP: &str = r#"[{"w":1,"t":1,"id":"A","name":"old title","cwd":"/x"}]"#;

    #[test]
    fn list_parses_windows_tabs_terminals() {
        let r = Rig::new(&[
            r#"[{"w":1,"t":1,"id":"A","name":"one","cwd":"/a"},{"w":2,"t":3,"id":"B","name":"two","cwd":null},{"w":2,"t":4,"id":"C","name":"three","cwd":""}]"#,
        ]);
        let tabs = Ghostty.list(&r.ports()).unwrap().unwrap();
        assert_eq!(Ghostty.id(), "ghostty");
        assert_eq!(tabs.len(), 3);
        assert_eq!(tabs[2].cwd, None);
        assert_eq!(tabs[0].ref_.as_deref(), Some("ghostty:A"));
        assert_eq!(tabs[0].terminal, "ghostty");
        assert_eq!(tabs[0].cwd.as_deref(), Some("/a"));
        assert_eq!((tabs[1].window, tabs[1].index), (2, 3));
        assert_eq!(tabs[1].cwd, None);
        assert_eq!(tabs[1].tty, None);
        let script = r.osa.scripts.borrow()[0].clone();
        assert!(script.contains("running()"));
        // Batched: no per-terminal property calls.
        assert!(script.contains("g.windows.tabs.terminals.id()"));
        assert!(script.contains("g.windows.tabs.terminals.name()"));
        assert!(script.contains("g.windows.tabs.terminals.workingDirectory()"));
        assert!(script.contains(r#"names[wi]?.[ti]?.[k] ?? """#));
        assert!(script.contains(r#"cwds[wi]?.[ti]?.[k] ?? """#));
        assert!(!script.contains("term.id()") && !script.contains("t.terminals()"));
        assert!(script.contains("try {") && script.contains("catch (e)"));
        assert!(r.ports().cmd.run("x", &[]).unwrap().is_empty());
    }

    #[test]
    fn list_not_running_is_none() {
        let r = Rig::new(&["null\n"]);
        assert!(Ghostty.list(&r.ports()).unwrap().is_none());
    }

    #[test]
    fn list_bad_output_errors() {
        let r = Rig::new(&["garbage"]);
        assert!(Ghostty.list(&r.ports()).is_err());
    }

    #[test]
    fn list_handles_quotes_newlines_emoji_in_titles() {
        let title = "say \"hi\"\nline2 \u{1F600} \\ end";
        let json = serde_json::json!([{"w":1,"t":1,"id":"A","name":title,"cwd":"/é"}]).to_string();
        let r = Rig::new(&[&json]);
        let tabs = Ghostty.list(&r.ports()).unwrap().unwrap();
        assert_eq!(tabs[0].title, title);
        assert_eq!(tabs[0].cwd.as_deref(), Some("/é"));
    }

    #[test]
    fn focus_ref_runs_focus_and_activate() {
        let r = Rig::new(&[r#""name""#]);
        Ghostty.focus(&r.ports(), "ghostty:AB\"C").unwrap();
        let s = r.osa.scripts.borrow()[0].clone();
        assert!(s.contains("g.focus(terminal)") && s.contains("g.activate()"));
        assert!(s.contains(r#""AB\"C""#));
        assert!(r.tty.0.borrow().is_empty());
    }

    #[test]
    fn focus_ref_rejects_foreign_ref() {
        let r = Rig::new(&[]);
        let e = Ghostty.focus(&r.ports(), "tmux:%1").unwrap_err();
        assert!(e.to_string().contains("not a Ghostty ref"));
        assert!(r.osa.scripts.borrow().is_empty());
    }

    #[test]
    fn focus_ref_unknown_errors() {
        let r = Rig::new(&["null"]);
        let e = Ghostty.focus(&r.ports(), "ghostty:nope").unwrap_err();
        assert_eq!(e.to_string(), "Ghostty tab not found");
    }

    #[test]
    fn focus_tty_not_running_is_false() {
        let r = Rig::new(&["null"]);
        assert!(!Ghostty.focus_tty(&r.ports(), "ttys001").unwrap());
        assert!(r.writes().is_empty());
    }

    #[test]
    fn focus_tty_hits_on_third_attempt_with_growing_sleeps() {
        let r = Rig::new(&[SNAP, "", "", "A\n", r#""changed""#]);
        assert!(Ghostty.focus_tty(&r.ports(), "ttys001").unwrap());
        assert_eq!(*r.sleep.0.borrow(), vec![50, 100, 150]);
        assert_eq!(
            r.writes(),
            vec![
                osc_title("csx-ttys001-1"),
                osc_title("csx-ttys001-2"),
                osc_title("csx-ttys001-3"),
            ]
        );
        assert!(r.osa.scripts.borrow()[1].contains(r#""csx-ttys001-1""#));
    }

    #[test]
    fn focus_tty_restores_snapshot_title_when_marker_remains() {
        let r = Rig::new(&[SNAP, "A", r#""csx-ttys001-1""#]);
        assert!(Ghostty.focus_tty(&r.ports(), "ttys001").unwrap());
        let w = r.writes();
        assert_eq!(w.len(), 2);
        assert_eq!(w[1], osc_title("old title"));
    }

    #[test]
    fn focus_tty_marker_remains_but_no_snapshot_title_skips_restore() {
        let r = Rig::new(&[SNAP, "ZZ", r#""csx-ttys001-1""#]);
        assert!(Ghostty.focus_tty(&r.ports(), "ttys001").unwrap());
        assert_eq!(r.writes().len(), 1);
    }

    #[test]
    fn focus_tty_does_not_restore_when_title_changed() {
        let r = Rig::new(&[SNAP, "A", r#""claude is thinking""#]);
        assert!(Ghostty.focus_tty(&r.ports(), "ttys001").unwrap());
        assert_eq!(r.writes().len(), 1);
    }

    #[test]
    fn focus_tty_gives_up_after_six_attempts() {
        let r = Rig::new(&[SNAP, "", "", "", "", "", ""]);
        assert!(!Ghostty.focus_tty(&r.ports(), "ttys001").unwrap());
        assert_eq!(r.writes().len(), MARKER_ATTEMPTS as usize);
        assert_eq!(*r.sleep.0.borrow(), vec![50, 100, 150, 200, 250, 300]);
    }

    #[test]
    fn focus_tty_hit_but_terminal_vanished_errors() {
        let r = Rig::new(&[SNAP, "A", "null"]);
        assert!(Ghostty.focus_tty(&r.ports(), "ttys001").is_err());
    }

    #[test]
    fn focus_tty_rejects_bad_tty_name() {
        let r = Rig::new(&[SNAP]);
        assert!(Ghostty.focus_tty(&r.ports(), "../x").is_err());
        assert!(r.writes().is_empty());
    }

    /// A folder with a quote and a line with a quote, a double quote and
    /// backslashes, as `resume_line` could build them.
    const CWD: &str = "/Users/me/it's here";
    const LINE: &str = r#"cd '/Users/me/it'\''s here' && echo "a\b""#;
    /// `LINE` as a JXA string literal.
    const LINE_LIT: &str = r#""cd '/Users/me/it'\\''s here' && echo \"a\\b\"""#;

    #[test]
    fn open_tab_in_running_ghostty_sends_one_escaped_script() {
        let l = log();
        let os = FakeOs::new(&l).osa("true").osa("ok");
        Ghostty.open_tab(&os.ports(), CWD, LINE).unwrap();
        assert_eq!(*l.borrow(), ["osa", "osa"]);
        let s = os.scripts.borrow()[1].clone();
        assert!(s.contains("const c = g.newSurfaceConfiguration();"), "{s}");
        assert!(s.contains(r#"c.initialWorkingDirectory = "/Users/me/it's here";"#));
        assert!(
            s.contains(&format!(r#"c.initialInput = {LINE_LIT} + "\r";"#)),
            "{s}"
        );
        assert!(s.contains("g.newTab({in: g.frontWindow(), withConfiguration: c});"));
        assert!(s.contains("g.newWindow({withConfiguration: c});"));
        assert!(s.contains("if (g.windows().length > 0)"));
        assert!(s.contains("g.activate();"));
    }

    #[test]
    fn open_tab_starts_ghostty_and_waits_for_it() {
        let l = log();
        let os = FakeOs::new(&l)
            .answer("open -b", "")
            .osa("false")
            .osa("false")
            .osa("true")
            .osa("ok");
        Ghostty.open_tab(&os.ports(), CWD, LINE).unwrap();
        assert_eq!(
            *l.borrow(),
            [
                "osa",
                "run:open -b com.mitchellh.ghostty",
                "sleep:250",
                "osa",
                "sleep:250",
                "osa",
                "osa"
            ]
        );
        assert!(os.scripts.borrow()[3].contains("newSurfaceConfiguration"));
    }

    #[test]
    fn open_tab_times_out_when_ghostty_does_not_start() {
        let l = log();
        // Every running() check answers "", which is not "true".
        let os = FakeOs::new(&l).answer("open -b", "");
        let e = Ghostty.open_tab(&os.ports(), CWD, LINE).unwrap_err();
        assert_eq!(e.to_string(), "Ghostty did not start");
        assert!(!os
            .scripts
            .borrow()
            .iter()
            .any(|s| s.contains("newSurfaceConfiguration")));
    }

    #[test]
    fn open_tab_script_failure_is_an_error() {
        let os = FakeOs::new(&log()).osa("true").osa_fail("not allowed");
        let e = Ghostty.open_tab(&os.ports(), CWD, LINE).unwrap_err();
        assert_eq!(e.to_string(), "not allowed");
    }

    #[test]
    fn osc_title_strips_control_chars() {
        assert_eq!(
            osc_title("a\x1b]0;b\x07c\nd\x7f"),
            b"\x1b]2;a]0;bcd\x07".to_vec()
        );
        assert_eq!(osc_title(""), b"\x1b]2;\x07".to_vec());
    }
}
