//! tmux as an inner layer: panes nested under the outer terminal tab that
//! hosts their tmux client, and focused with switch-client/select-window/
//! select-pane.

use super::{CommandRunner, Tab};
use crate::error::{Error, Result};

/// One tmux pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmuxPane {
    /// Pane id, for example `%3`.
    pub id: String,
    /// Session the pane belongs to.
    pub session: String,
    /// Window index in the session.
    pub window: u32,
    /// Pane index in the window.
    pub pane: u32,
    /// Pane tty without `/dev/`.
    pub tty: String,
    /// Current working directory of the pane.
    pub cwd: String,
    /// Pane title.
    pub title: String,
}

/// One tmux client (an attached terminal).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmuxClient {
    /// Client tty without `/dev/`.
    pub tty: String,
    /// Session the client is attached to.
    pub session: String,
}

const PANE_FORMAT: &str = "#{pane_id}\t#{session_name}\t#{window_index}\t#{pane_index}\t#{pane_tty}\t#{pane_current_path}\t#{pane_title}";
const CLIENT_FORMAT: &str = "#{client_tty}\t#{session_name}";

fn strip_dev(tty: &str) -> String {
    tty.strip_prefix("/dev/").unwrap_or(tty).to_string()
}

fn parse_pane(line: &str) -> Option<TmuxPane> {
    let f: Vec<&str> = line.splitn(7, '\t').collect();
    if f.len() < 7 {
        return None;
    }
    Some(TmuxPane {
        id: f[0].to_string(),
        session: f[1].to_string(),
        window: f[2].parse().ok()?,
        pane: f[3].parse().ok()?,
        tty: strip_dev(f[4]),
        cwd: f[5].to_string(),
        title: f[6].to_string(),
    })
}

fn parse_client(line: &str) -> Option<TmuxClient> {
    let (tty, session) = line.split_once('\t')?;
    Some(TmuxClient {
        tty: strip_dev(tty),
        session: session.to_string(),
    })
}

/// List panes and clients. `Ok(None)` when no tmux server answers.
pub fn list(cmd: &dyn CommandRunner) -> Result<Option<(Vec<TmuxPane>, Vec<TmuxClient>)>> {
    let Ok(out) = cmd.run("tmux", &["list-panes", "-a", "-F", PANE_FORMAT]) else {
        return Ok(None);
    };
    let panes = out.lines().filter_map(parse_pane).collect();
    let clients = cmd
        .run("tmux", &["list-clients", "-F", CLIENT_FORMAT])
        .map(|o| o.lines().filter_map(parse_client).collect())
        .unwrap_or_default();
    Ok(Some((panes, clients)))
}

fn pane_tab(p: &TmuxPane, ref_: Option<String>, window: u32, index: u32) -> Tab {
    Tab {
        terminal: "tmux".into(),
        ref_,
        title: p.title.clone(),
        cwd: Some(p.cwd.clone()),
        tty: Some(p.tty.clone()),
        window,
        index,
        agent: None,
    }
}

/// Panes as tabs: each directly after the outer tab whose tty is a client tty
/// of the pane's session (nested panes take that outer tab's window and index).
/// Panes of a session with no matching outer tab keep the window and pane
/// index from tmux and are appended at the end. They still get a ref when
/// their session has a client, because that client may run in a tab whose tty
/// no backend reports (Ghostty), which `tty:` focus can still find. Panes of a
/// session with no client get no ref.
pub fn nest(outer: Vec<Tab>, panes: &[TmuxPane], clients: &[TmuxClient]) -> Vec<Tab> {
    let mut placed = vec![false; panes.len()];
    let mut out = Vec::with_capacity(outer.len() + panes.len());
    for tab in outer {
        let sessions: Vec<&str> = clients
            .iter()
            .filter(|c| tab.tty.as_deref() == Some(c.tty.as_str()))
            .map(|c| c.session.as_str())
            .collect();
        let mut nested = Vec::new();
        for (i, p) in panes.iter().enumerate() {
            if !placed[i] && sessions.contains(&p.session.as_str()) {
                placed[i] = true;
                nested.push(pane_tab(
                    p,
                    Some(format!("tmux:{}", p.id)),
                    tab.window,
                    tab.index,
                ));
            }
        }
        out.push(tab);
        out.extend(nested);
    }
    for (p, _) in panes.iter().zip(&placed).filter(|(_, done)| !**done) {
        let attached = clients.iter().any(|c| c.session == p.session);
        let ref_ = attached.then(|| format!("tmux:{}", p.id));
        out.push(pane_tab(p, ref_, p.window, p.pane));
    }
    out
}

/// Client ttys of the pane's session, in `list-clients` order. Errors when the
/// pane is gone or its session has no attached client.
pub fn client_ttys(
    pane_id: &str,
    clients: &[TmuxClient],
    panes: &[TmuxPane],
) -> Result<Vec<String>> {
    let pane = panes
        .iter()
        .find(|p| p.id == pane_id)
        .ok_or_else(|| Error::other(format!("tmux pane {pane_id} not found")))?;
    let ttys: Vec<String> = clients
        .iter()
        .filter(|c| c.session == pane.session)
        .map(|c| c.tty.clone())
        .collect();
    if ttys.is_empty() {
        return Err(Error::other(format!(
            "tmux session {} has no attached client",
            pane.session
        )));
    }
    Ok(ttys)
}

/// Switch the client on `client_tty` to the pane, then select its window and
/// the pane itself.
pub fn focus_client(cmd: &dyn CommandRunner, pane_id: &str, client_tty: &str) -> Result<()> {
    let dev = format!("/dev/{client_tty}");
    cmd.run("tmux", &["switch-client", "-c", &dev, "-t", pane_id])?;
    cmd.run("tmux", &["select-window", "-t", pane_id])?;
    cmd.run("tmux", &["select-pane", "-t", pane_id])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    struct Fake {
        panes: Option<String>,
        clients: Option<String>,
        fail_on: Option<&'static str>,
        calls: RefCell<Vec<Vec<String>>>,
    }

    impl Fake {
        fn new(panes: Option<&str>, clients: Option<&str>) -> Self {
            Fake {
                panes: panes.map(str::to_string),
                clients: clients.map(str::to_string),
                fail_on: None,
                calls: RefCell::new(vec![]),
            }
        }
    }

    impl CommandRunner for Fake {
        fn run(&self, program: &str, args: &[&str]) -> Result<String> {
            assert_eq!(program, "tmux");
            self.calls
                .borrow_mut()
                .push(args.iter().map(|s| s.to_string()).collect());
            if self.fail_on == Some(args[0]) {
                return Err(Error::other("fail"));
            }
            let r = match args[0] {
                "list-panes" => self.panes.clone(),
                "list-clients" => self.clients.clone(),
                _ => Some(String::new()),
            };
            r.ok_or_else(|| Error::other("no server"))
        }
    }

    fn pane(id: &str, session: &str, window: u32, p: u32) -> TmuxPane {
        TmuxPane {
            id: id.into(),
            session: session.into(),
            window,
            pane: p,
            tty: format!("ttys9{p}"),
            cwd: "/w".into(),
            title: format!("t{id}"),
        }
    }

    fn client(tty: &str, session: &str) -> TmuxClient {
        TmuxClient {
            tty: tty.into(),
            session: session.into(),
        }
    }

    fn outer(tty: &str, window: u32, index: u32) -> Tab {
        Tab {
            terminal: "ghostty".into(),
            ref_: Some(format!("g{index}")),
            tty: Some(tty.into()),
            window,
            index,
            ..Tab::default()
        }
    }

    #[test]
    fn list_parses_tab_separated() {
        let f = Fake::new(
            Some("%3\tmain\t1\t0\t/dev/ttys005\t/w/a\tmy title\nbad line\n%4\tmain\tx\t0\t/dev/t\t/\tt\n%5\ts\t2\t1\tttys006\t/w\ttab\tin title\n"),
            Some("/dev/ttys001\tmain\nnotab\n"),
        );
        let (panes, clients) = list(&f).unwrap().unwrap();
        assert_eq!(panes.len(), 2);
        assert_eq!(panes[0].id, "%3");
        assert_eq!(panes[0].tty, "ttys005");
        assert_eq!(panes[0].title, "my title");
        assert_eq!((panes[0].window, panes[0].pane), (1, 0));
        assert_eq!(panes[1].tty, "ttys006");
        assert_eq!(panes[1].title, "tab\tin title");
        assert_eq!(clients, vec![client("ttys001", "main")]);
        let calls = f.calls.borrow();
        assert_eq!(&calls[0][..3], ["list-panes", "-a", "-F"]);
        assert!(calls[0][3].starts_with("#{pane_id}\t#{session_name}"));
        assert_eq!(calls[1][0], "list-clients");
    }

    #[test]
    fn no_server_is_none() {
        assert_eq!(list(&Fake::new(None, None)).unwrap(), None);
    }

    #[test]
    fn list_clients_failure_means_no_clients() {
        let (panes, clients) = list(&Fake::new(Some("%1\ts\t0\t0\tttys1\t/\tt\n"), None))
            .unwrap()
            .unwrap();
        assert_eq!(panes.len(), 1);
        assert!(clients.is_empty());
    }

    #[test]
    fn nest_places_panes_after_client_tab() {
        let outs = vec![
            outer("ttys001", 1, 1),
            outer("ttys002", 1, 2),
            outer("ttys003", 2, 1),
        ];
        let panes = vec![
            pane("%1", "a", 0, 0),
            pane("%2", "a", 0, 1),
            pane("%3", "b", 1, 0),
        ];
        let clients = vec![
            client("ttys001", "a"),
            client("ttys003", "b"),
            client("ttys002", "a"),
        ];
        let rows = nest(outs, &panes, &clients);
        let order: Vec<(&str, Option<&str>)> = rows
            .iter()
            .map(|t| (t.terminal.as_str(), t.ref_.as_deref()))
            .collect();
        assert_eq!(
            order,
            vec![
                ("ghostty", Some("g1")),
                ("tmux", Some("tmux:%1")),
                ("tmux", Some("tmux:%2")),
                ("ghostty", Some("g2")),
                ("ghostty", Some("g1")),
                ("tmux", Some("tmux:%3")),
            ]
        );
        assert_eq!((rows[1].window, rows[1].index), (1, 1));
        assert_eq!(rows[1].title, "t%1");
        assert_eq!(rows[1].cwd.as_deref(), Some("/w"));
        assert_eq!(rows[1].tty.as_deref(), Some("ttys90"));
        assert_eq!((rows[5].window, rows[5].index), (2, 1));
    }

    #[test]
    fn nest_unplaced_pane_with_a_client_keeps_its_ref() {
        // The client runs in a tab whose tty no backend reports (Ghostty), so
        // the pane is not nested but can still be focused through its client.
        let panes = vec![pane("%5", "g", 2, 1)];
        let rows = nest(
            vec![outer("ttys001", 1, 1)],
            &panes,
            &[client("ttys001", "x"), client("ttys008", "g")],
        );
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].ref_.as_deref(), Some("tmux:%5"));
        assert_eq!((rows[1].window, rows[1].index), (2, 1));
    }

    #[test]
    fn nest_pane_without_client_has_no_ref() {
        let panes = vec![pane("%7", "lonely", 3, 2)];
        let rows = nest(
            vec![outer("ttys001", 1, 1)],
            &panes,
            &[client("ttys001", "x")],
        );
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].terminal, "tmux");
        assert_eq!(rows[1].ref_, None);
        assert_eq!((rows[1].window, rows[1].index), (3, 2));
    }

    #[test]
    fn client_ttys_lists_every_client_of_the_session_in_order() {
        let panes = vec![pane("%3", "main", 1, 0)];
        let clients = vec![
            client("ttys004", "main"),
            client("ttys001", "other"),
            client("ttys002", "main"),
        ];
        assert_eq!(
            client_ttys("%3", &clients, &panes).unwrap(),
            vec!["ttys004", "ttys002"]
        );
        let e = client_ttys("%3", &clients[1..2], &panes).unwrap_err();
        assert!(e.to_string().contains("no attached client"));
        let e = client_ttys("%9", &clients, &panes).unwrap_err();
        assert!(e.to_string().contains("%9 not found"));
    }

    #[test]
    fn focus_client_switches_that_client() {
        let f = Fake::new(None, None);
        focus_client(&f, "%3", "ttys002").unwrap();
        assert_eq!(
            *f.calls.borrow(),
            vec![
                vec!["switch-client", "-c", "/dev/ttys002", "-t", "%3"],
                vec!["select-window", "-t", "%3"],
                vec!["select-pane", "-t", "%3"],
            ]
        );
        for step in ["switch-client", "select-window", "select-pane"] {
            let mut f = Fake::new(None, None);
            f.fail_on = Some(step);
            assert!(focus_client(&f, "%3", "ttys002").is_err(), "{step}");
        }
    }
}
