//! Orchestration of terminal backends and agents: the list pipeline that joins
//! tabs, tmux panes and agents into rows, and `focus` routing by ref.

use super::ghostty::Ghostty;
use super::iterm2::ITerm2;
use super::kitty::Kitty;
use super::process::ProcessTable;
use super::terminal_app::TerminalApp;
use super::wezterm::WezTerm;
use super::TerminalBackend;
use super::{matches_query, sort_rows, tmux, AgentInfo, Ports, Row, Tab, TabsReport};
use crate::agent::{self, Agent, AgentContext};
use crate::error::{Error, Result};

/// What to list.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ListOptions {
    /// Keep only rows that have an agent.
    pub agents_only: bool,
    /// Terminal ids to keep; empty keeps every terminal.
    pub terminals: Vec<String>,
    /// Query words; every word must match a row.
    pub query: Vec<String>,
}

/// Every supported terminal backend: ghostty, iterm2, terminal, kitty, wezterm.
///
/// `kitty_socket` is an explicit kitty socket; `tmpdir` is the user's `TMPDIR`,
/// under which kitty's per-pid sockets are looked for.
pub fn backends(
    kitty_socket: Option<String>,
    tmpdir: Option<String>,
) -> Vec<Box<dyn TerminalBackend>> {
    vec![
        Box::new(Ghostty),
        Box::new(ITerm2),
        Box::new(TerminalApp),
        Box::new(Kitty {
            socket: kitty_socket,
            tmpdir,
        }),
        Box::new(WezTerm),
    ]
}

/// Ids of the terminal backends, whose refs start with `<id>:`.
const TERMINAL_IDS: &[&str] = &["ghostty", "iterm2", "terminal", "kitty", "wezterm"];

/// True when the terminal filter keeps `id`.
fn wants(opts: &ListOptions, id: &str) -> bool {
    opts.terminals.is_empty() || opts.terminals.iter().any(|t| t == id)
}

/// `"<id>: <error>"`, without repeating an id the message already starts with.
fn warning(id: &str, e: &Error) -> String {
    let msg = e.to_string();
    if msg.starts_with(&format!("{id}:")) {
        msg
    } else {
        format!("{id}: {msg}")
    }
}

/// List tabs and agents as sorted rows, with warnings for backends that failed.
///
/// Outer tabs are sorted before tmux panes are nested under them, so each pane
/// stays right after its outer tab. Agent rows then move to the top.
pub fn list(
    ports: &Ports,
    backends: &[Box<dyn TerminalBackend>],
    agents: &[Box<dyn Agent>],
    ctx: &AgentContext,
    opts: &ListOptions,
) -> TabsReport {
    let mut warnings = Vec::new();
    // tmux panes need every outer tab to find the one that hosts their client.
    let with_tmux = wants(opts, "tmux");
    let mut outer: Vec<Tab> = Vec::new();
    for b in backends {
        let selected = wants(opts, b.id());
        if !selected && !with_tmux {
            continue;
        }
        match b.list(ports) {
            Ok(tabs) => outer.extend(tabs.unwrap_or_default()),
            Err(e) if selected => warnings.push(warning(b.id(), &e)),
            Err(_) => {}
        }
    }
    sort_rows(&mut outer);
    let mut rows = outer;
    if with_tmux {
        if let Some((panes, clients)) = tmux::list(ports.cmd).ok().flatten() {
            rows = tmux::nest(rows, &panes, &clients);
        }
    }
    let found = match ProcessTable::load(ports.cmd) {
        Ok(table) => agent::detect(&table, agents, ctx),
        Err(e) => {
            warnings.push(format!("ps: {e}"));
            Vec::new()
        }
    };
    let left = attach_by_tty(&mut rows, found);
    let left = attach_ghostty_by_title(&mut rows, left);
    rows.extend(left.into_iter().map(agent_row));
    rows.retain(|r| {
        (opts.terminals.is_empty() || opts.terminals.contains(&r.terminal))
            && (!opts.agents_only || r.agent.is_some())
            && matches_query(r, &opts.query)
    });
    TabsReport {
        rows: agents_first(rows),
        warnings,
    }
}

/// Attach each agent to the first free row on its tty; return the rest.
fn attach_by_tty(rows: &mut [Row], agents: Vec<AgentInfo>) -> Vec<AgentInfo> {
    let mut left = Vec::new();
    for a in agents {
        let slot = rows
            .iter_mut()
            .find(|r| r.agent.is_none() && r.tty.is_some() && r.tty == a.tty);
        match slot {
            Some(r) => r.agent = Some(a),
            None => left.push(a),
        }
    }
    left
}

/// A title without its leading glyphs and spaces (anything that is not a
/// letter or a digit), such as Claude Code's `✳ ` status mark.
fn strip_glyphs(s: &str) -> &str {
    s.trim_start_matches(|c: char| !c.is_alphanumeric())
}

/// Ghostty reports no tty, so attach an agent to a Ghostty tab when their
/// stripped titles match uniquely on both sides. Return the agents left over.
fn attach_ghostty_by_title(rows: &mut [Row], agents: Vec<AgentInfo>) -> Vec<AgentInfo> {
    let keys: Vec<Option<String>> = rows
        .iter()
        .map(|r| {
            let k = strip_glyphs(&r.title);
            (r.terminal == "ghostty" && r.agent.is_none() && !k.is_empty()).then(|| k.to_string())
        })
        .collect();
    let mut agents: Vec<Option<AgentInfo>> = agents.into_iter().map(Some).collect();
    for (i, key) in keys.iter().enumerate() {
        let Some(key) = key else { continue };
        if keys.iter().filter(|k| k.as_ref() == Some(key)).count() != 1 {
            continue;
        }
        let hits: Vec<usize> = agents
            .iter()
            .enumerate()
            .filter(|(_, a)| {
                a.as_ref()
                    .and_then(|a| a.title.as_deref())
                    .is_some_and(|t| strip_glyphs(t) == key)
            })
            .map(|(j, _)| j)
            .collect();
        if let [j] = hits[..] {
            rows[i].agent = agents[j].take();
        }
    }
    agents.into_iter().flatten().collect()
}

/// A row for an agent with no tab: terminal `""`, its tty and cwd, and a
/// `tty:` ref when it has a tty.
fn agent_row(a: AgentInfo) -> Row {
    Tab {
        ref_: a.tty.as_ref().map(|t| format!("tty:{t}")),
        cwd: a.cwd.clone(),
        tty: a.tty.clone(),
        agent: Some(a),
        ..Tab::default()
    }
}

/// Agent rows first, sorted with [`sort_rows`]; the other rows keep their
/// order, which holds tmux panes under their outer tab.
fn agents_first(rows: Vec<Row>) -> Vec<Row> {
    let (mut with, without): (Vec<Row>, Vec<Row>) =
        rows.into_iter().partition(|r| r.agent.is_some());
    sort_rows(&mut with);
    with.extend(without);
    with
}

/// Focus the tab named by `tab_ref`.
///
/// - `ghostty:`, `iterm2:`, `terminal:`, `kitty:`, `wezterm:` go to that
///   backend.
/// - `tmux:<pane>` switches a client of the pane's session to the pane and
///   focuses the outer tab on that client's tty, trying each client in turn.
/// - `tty:<tty>` asks each backend for the tab on that tty.
pub fn focus(ports: &Ports, backends: &[Box<dyn TerminalBackend>], tab_ref: &str) -> Result<()> {
    let unknown = || Error::other(format!("unknown tab ref: {tab_ref}"));
    let (prefix, rest) = tab_ref.split_once(':').ok_or_else(unknown)?;
    match prefix {
        "tty" => focus_tty(ports, backends, rest),
        "tmux" => focus_tmux(ports, backends, rest),
        _ => match backends.iter().find(|b| b.id() == prefix) {
            Some(b) => b.focus(ports, tab_ref),
            None if TERMINAL_IDS.contains(&prefix) => {
                Err(Error::other(format!("{prefix} is not enabled")))
            }
            None => Err(unknown()),
        },
    }
}

fn focus_tmux(ports: &Ports, backends: &[Box<dyn TerminalBackend>], pane_id: &str) -> Result<()> {
    let Some((panes, clients)) = tmux::list(ports.cmd).ok().flatten() else {
        return Err(Error::other("tmux is not running"));
    };
    let mut result = Err(Error::other("no tmux client"));
    for tty in tmux::client_ttys(pane_id, &clients, &panes)? {
        result = tmux::focus_client(ports.cmd, pane_id, &tty)
            .and_then(|()| focus_tty(ports, backends, &tty));
        if result.is_ok() {
            break;
        }
    }
    result
}

/// Backends in the order `tty:` focus tries them: those that report a tty
/// first, Ghostty last, because Ghostty finds a tty by writing a marker title
/// to it, which would rename a tab of another app and costs up to a second.
fn tty_focus_order(backends: &[Box<dyn TerminalBackend>]) -> Vec<&dyn TerminalBackend> {
    let mut v: Vec<&dyn TerminalBackend> = backends.iter().map(|b| b.as_ref()).collect();
    v.sort_by_key(|b| b.id() == "ghostty");
    v
}

fn focus_tty(ports: &Ports, backends: &[Box<dyn TerminalBackend>], tty: &str) -> Result<()> {
    let table = ProcessTable::load(ports.cmd)?;
    if table.on_tty(tty).is_empty() {
        return Err(Error::other(format!(
            "the session in /dev/{tty} is no longer running"
        )));
    }
    let mut errors = Vec::new();
    for b in tty_focus_order(backends) {
        match b.focus_tty(ports, tty) {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(e) => errors.push(format!("{}: {e}", b.id())),
        }
    }
    let mut msg = format!("no supported terminal shows /dev/{tty}");
    if !errors.is_empty() {
        msg = format!("{msg} ({})", errors.join("; "));
    }
    Err(Error::other(msg))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tabs::test_support::{
        agent, ctx, log, ps_line, tab, with_tty, FakeAgent, FakeBackend, FakeOs, Log,
    };

    fn ids(rows: &[Row]) -> Vec<String> {
        rows.iter()
            .map(|r| r.ref_.clone().unwrap_or_else(|| "-".into()))
            .collect()
    }

    fn opts() -> ListOptions {
        ListOptions::default()
    }

    fn run_list(
        os: &FakeOs,
        backends: Vec<Box<dyn TerminalBackend>>,
        infos: Vec<AgentInfo>,
        opts: &ListOptions,
    ) -> TabsReport {
        let agents: Vec<Box<dyn Agent>> = vec![Box::new(FakeAgent { infos })];
        list(&os.ports(), &backends, &agents, &ctx(), opts)
    }

    fn all_fake_backends(l: &Log) -> Vec<FakeBackend> {
        ["ghostty", "iterm2", "terminal", "kitty", "wezterm"]
            .into_iter()
            .map(|id| FakeBackend::new(id, l))
            .collect()
    }

    fn logged(l: &Log, prefix: &str) -> Vec<String> {
        l.borrow()
            .iter()
            .filter(|e| e.starts_with(prefix))
            .cloned()
            .collect()
    }

    #[test]
    fn backends_lists_every_terminal_in_order() {
        let b = backends(Some("unix:/tmp/k".into()), None);
        let ids: Vec<&str> = b.iter().map(|b| b.id()).collect();
        assert_eq!(ids, ["ghostty", "iterm2", "terminal", "kitty", "wezterm"]);
    }

    #[test]
    fn list_attaches_agent_by_tty() {
        let l = log();
        let os = FakeOs::new(&l).answer("ps -axo", &ps_line(10, "ttys002"));
        let it = FakeBackend::new("iterm2", &l).tabs(vec![
            with_tty(tab("iterm2", "iterm2:a", "shell", 1, 1), "ttys001"),
            with_tty(tab("iterm2", "iterm2:b", "claude", 1, 2), "ttys002"),
        ]);
        let r = run_list(&os, vec![it.boxed()], vec![agent(10, None, None)], &opts());
        assert_eq!(ids(&r.rows), ["iterm2:b", "iterm2:a"]);
        assert_eq!(r.rows[0].agent.as_ref().unwrap().pid, 10);
        assert!(r.rows[1].agent.is_none());
        assert!(r.warnings.is_empty());
    }

    #[test]
    fn list_attaches_ghostty_agent_by_stripped_title() {
        let l = log();
        let ps = [
            ps_line(10, "ttys005"),
            ps_line(11, "ttys006"),
            ps_line(12, "ttys007"),
        ]
        .concat();
        let os = FakeOs::new(&l).answer("ps -axo", &ps);
        let g = FakeBackend::new("ghostty", &l).tabs(vec![
            tab("ghostty", "ghostty:1", "✳ Fix parser", 1, 1),
            tab("ghostty", "ghostty:2", "⠂ Other", 1, 2),
            tab("ghostty", "ghostty:3", "✳ ", 1, 3),
        ]);
        let infos = vec![
            agent(10, Some("Fix parser"), Some(30)),
            agent(11, Some("✳ Other"), Some(20)),
            agent(12, Some(""), Some(10)),
        ];
        let r = run_list(&os, vec![g.boxed()], infos, &opts());
        assert_eq!(
            ids(&r.rows),
            ["ghostty:1", "ghostty:2", "tty:ttys007", "ghostty:3"]
        );
        assert_eq!(r.rows[0].agent.as_ref().unwrap().pid, 10);
        assert_eq!(r.rows[1].agent.as_ref().unwrap().pid, 11);
        assert!(r.rows[3].agent.is_none());
    }

    #[test]
    fn ghostty_ambiguous_title_falls_back_to_tty_ref() {
        let l = log();
        let ps = [
            ps_line(10, "ttys007"),
            ps_line(11, "ttys008"),
            ps_line(12, "ttys009"),
        ]
        .concat();
        let os = FakeOs::new(&l).answer("ps -axo", &ps);
        let g = FakeBackend::new("ghostty", &l).tabs(vec![
            tab("ghostty", "ghostty:1", "✳ Same", 1, 1),
            tab("ghostty", "ghostty:2", "⠂ Same", 1, 2),
            tab("ghostty", "ghostty:3", "Dup", 1, 3),
        ]);
        let infos = vec![
            agent(10, Some("Same"), Some(30)),
            agent(11, Some("Dup"), Some(20)),
            agent(12, Some("Dup"), Some(10)),
        ];
        let r = run_list(&os, vec![g.boxed()], infos, &opts());
        assert_eq!(
            ids(&r.rows),
            [
                "tty:ttys007",
                "tty:ttys008",
                "tty:ttys009",
                "ghostty:1",
                "ghostty:2",
                "ghostty:3"
            ]
        );
        assert!(r.rows[3..].iter().all(|r| r.agent.is_none()));
    }

    #[test]
    fn list_unattached_agent_becomes_row_and_tty_less_agent_is_dropped() {
        let l = log();
        let ps = format!("{}11 1 11 11 ?? 00:01 agent\n", ps_line(10, "ttys003"));
        let os = FakeOs::new(&l).answer("ps -axo", &ps);
        let mut a = agent(10, Some("t"), Some(20));
        a.cwd = Some("/w".into());
        let b = vec![FakeBackend::new("iterm2", &l).boxed()];
        let r = run_list(&os, b, vec![agent(11, None, Some(10)), a], &opts());
        assert_eq!(ids(&r.rows), ["tty:ttys003"]);
        let row = &r.rows[0];
        assert_eq!(row.terminal, "");
        assert_eq!(row.tty.as_deref(), Some("ttys003"));
        assert_eq!(row.cwd.as_deref(), Some("/w"));
        assert_eq!(row.title, "");
    }

    #[test]
    fn list_agents_only_filters() {
        let l = log();
        let ps = [ps_line(10, "ttys002"), ps_line(11, "ttys009")].concat();
        let os = FakeOs::new(&l).answer("ps -axo", &ps);
        let it = FakeBackend::new("iterm2", &l).tabs(vec![
            with_tty(tab("iterm2", "iterm2:a", "shell", 1, 1), "ttys001"),
            with_tty(tab("iterm2", "iterm2:b", "claude", 1, 2), "ttys002"),
        ]);
        let o = ListOptions {
            agents_only: true,
            ..opts()
        };
        let infos = vec![agent(10, None, Some(2)), agent(11, None, Some(1))];
        let r = run_list(&os, vec![it.boxed()], infos, &o);
        assert_eq!(ids(&r.rows), ["iterm2:b", "tty:ttys009"]);
    }

    #[test]
    fn list_query_filters_rows() {
        let l = log();
        let os = FakeOs::new(&l).answer("ps -axo", &ps_line(10, "ttys002"));
        let it = FakeBackend::new("iterm2", &l).tabs(vec![
            with_tty(tab("iterm2", "iterm2:a", "shell", 1, 1), "ttys001"),
            with_tty(tab("iterm2", "iterm2:b", "claude", 1, 2), "ttys002"),
        ]);
        let o = ListOptions {
            query: vec!["PARSER".into()],
            ..opts()
        };
        let infos = vec![agent(10, Some("Fix parser"), None)];
        let r = run_list(&os, vec![it.boxed()], infos, &o);
        assert_eq!(ids(&r.rows), ["iterm2:b"]);
    }

    #[test]
    fn list_terminal_filter() {
        let l = log();
        let os = FakeOs::new(&l)
            .answer("ps -axo", &ps_line(10, "ttys009"))
            .answer("tmux list-panes", "%1\ta\t0\t0\t/dev/ttys101\t/w\tpane\n")
            .answer("tmux list-clients", "/dev/ttys001\ta\n");
        let mk = || {
            vec![
                FakeBackend::new("ghostty", &l).failing("boom").boxed(),
                FakeBackend::new("iterm2", &l)
                    .tabs(vec![with_tty(
                        tab("iterm2", "iterm2:a", "s", 1, 1),
                        "ttys001",
                    )])
                    .boxed(),
            ]
        };
        let only = |t: &str| ListOptions {
            terminals: vec![t.into()],
            ..opts()
        };
        let infos = || vec![agent(10, None, None)];
        let r = run_list(&os, mk(), infos(), &only("iterm2"));
        assert_eq!(ids(&r.rows), ["iterm2:a"]);
        assert!(r.warnings.is_empty());
        assert!(logged(&l, "run:tmux").is_empty());
        let r = run_list(&os, mk(), infos(), &only("tmux"));
        assert_eq!(ids(&r.rows), ["tmux:%1"]);
        assert!(r.warnings.is_empty());
        let r = run_list(&os, mk(), infos(), &only("ghostty"));
        assert!(r.rows.is_empty());
        assert_eq!(r.warnings, ["ghostty: boom"]);
    }

    #[test]
    fn list_continues_after_backend_error() {
        let l = log();
        let os = FakeOs::new(&l).answer("ps -axo", "");
        let b = vec![
            FakeBackend::new("ghostty", &l)
                .failing("osascript failed: not allowed")
                .boxed(),
            FakeBackend::new("iterm2", &l)
                .tabs(vec![tab("iterm2", "iterm2:a", "s", 1, 1)])
                .boxed(),
            FakeBackend::new("kitty", &l)
                .failing("kitty: turn on allow_remote_control")
                .boxed(),
            FakeBackend::new("wezterm", &l).boxed(),
        ];
        let r = run_list(&os, b, vec![], &opts());
        assert_eq!(ids(&r.rows), ["iterm2:a"]);
        assert_eq!(
            r.warnings,
            [
                "ghostty: osascript failed: not allowed",
                "kitty: turn on allow_remote_control"
            ]
        );
    }

    #[test]
    fn list_warns_when_ps_fails() {
        let l = log();
        let os = FakeOs::new(&l).fail("ps -axo", "ps failed: denied");
        let b = vec![FakeBackend::new("iterm2", &l)
            .tabs(vec![tab("iterm2", "iterm2:a", "s", 1, 1)])
            .boxed()];
        let r = run_list(&os, b, vec![agent(1, None, None)], &opts());
        assert_eq!(ids(&r.rows), ["iterm2:a"]);
        assert_eq!(r.warnings, ["ps: ps failed: denied"]);
    }

    #[test]
    fn list_nests_tmux_panes() {
        // Backends list out of order; outer rows are sorted before the panes
        // are nested, so each pane stays right after its outer tab. A pane with
        // an agent still moves to the top with the other agent rows.
        let l = log();
        let os = FakeOs::new(&l)
            .answer("ps -axo", &ps_line(10, "ttys102"))
            .answer(
                "tmux list-panes",
                "%1\ta\t0\t0\t/dev/ttys101\t/w\tp1\n%2\ta\t0\t1\t/dev/ttys102\t/w\tp2\n%3\tz\t4\t2\t/dev/ttys103\t/w\tp3\n",
            )
            .answer("tmux list-clients", "/dev/ttys001\ta\n");
        let b = vec![
            FakeBackend::new("iterm2", &l)
                .tabs(vec![
                    with_tty(tab("iterm2", "iterm2:w2", "s", 2, 1), "ttys002"),
                    with_tty(tab("iterm2", "iterm2:w1", "s", 1, 1), "ttys001"),
                ])
                .boxed(),
            FakeBackend::new("ghostty", &l)
                .tabs(vec![tab("ghostty", "ghostty:1", "g", 1, 1)])
                .boxed(),
        ];
        let r = run_list(&os, b, vec![agent(10, None, Some(1))], &opts());
        assert_eq!(
            ids(&r.rows),
            [
                "tmux:%2",
                "ghostty:1",
                "iterm2:w1",
                "tmux:%1",
                "iterm2:w2",
                "-"
            ]
        );
        assert_eq!(r.rows[0].agent.as_ref().unwrap().pid, 10);
        assert_eq!((r.rows[3].window, r.rows[3].index), (1, 1));
        assert_eq!(r.rows[5].terminal, "tmux");
    }

    #[test]
    fn focus_routes_each_prefix() {
        let l = log();
        let os = FakeOs::new(&l);
        let b: Vec<Box<dyn TerminalBackend>> = all_fake_backends(&l)
            .into_iter()
            .map(FakeBackend::boxed)
            .collect();
        for id in ["ghostty", "iterm2", "terminal", "kitty", "wezterm"] {
            focus(&os.ports(), &b, &format!("{id}:x")).unwrap();
        }
        assert_eq!(
            *l.borrow(),
            [
                "focus:ghostty:ghostty:x",
                "focus:iterm2:iterm2:x",
                "focus:terminal:terminal:x",
                "focus:kitty:kitty:x",
                "focus:wezterm:wezterm:x"
            ]
        );
    }

    #[test]
    fn focus_tmux_then_outer_tty() {
        let l = log();
        let os = FakeOs::new(&l)
            .answer("ps -axo", &ps_line(5, "ttys001"))
            .answer("tmux list-panes", "%1\ta\t0\t0\t/dev/ttys101\t/w\tp\n")
            .answer("tmux list-clients", "/dev/ttys001\ta\n")
            .answer("tmux switch-client", "")
            .answer("tmux select-window", "")
            .answer("tmux select-pane", "");
        let b = vec![FakeBackend::new("iterm2", &l).shows("ttys001").boxed()];
        focus(&os.ports(), &b, "tmux:%1").unwrap();
        let calls: Vec<String> = l.borrow().clone();
        assert_eq!(
            calls[2..],
            [
                "run:tmux switch-client -c /dev/ttys001 -t %1",
                "run:tmux select-window -t %1",
                "run:tmux select-pane -t %1",
                "run:ps -axo pid=,ppid=,pgid=,tpgid=,tty=,etime=,args=",
                "focus_tty:iterm2:ttys001",
            ]
        );
    }

    #[test]
    fn focus_tmux_tries_each_client_tty() {
        let l = log();
        let tmux_os = || {
            FakeOs::new(&l)
                .answer(
                    "ps -axo",
                    &[ps_line(5, "ttys001"), ps_line(6, "ttys003")].concat(),
                )
                .answer("tmux list-panes", "%1\ta\t0\t0\t/dev/ttys101\t/w\tp\n")
                .answer(
                    "tmux list-clients",
                    "/dev/ttys001\ta\n/dev/ttys002\tb\n/dev/ttys003\ta\n",
                )
                .answer("tmux select-window", "")
                .answer("tmux select-pane", "")
        };
        let os = tmux_os().answer("tmux switch-client", "");
        let b = vec![FakeBackend::new("iterm2", &l).shows("ttys003").boxed()];
        focus(&os.ports(), &b, "tmux:%1").unwrap();
        assert_eq!(
            logged(&l, "run:tmux switch-client"),
            [
                "run:tmux switch-client -c /dev/ttys001 -t %1",
                "run:tmux switch-client -c /dev/ttys003 -t %1"
            ]
        );
        assert_eq!(
            logged(&l, "focus_tty"),
            ["focus_tty:iterm2:ttys001", "focus_tty:iterm2:ttys003"]
        );

        let none = vec![FakeBackend::new("iterm2", &l).boxed()];
        let e = focus(&os.ports(), &none, "tmux:%1").unwrap_err();
        assert_eq!(e.to_string(), "no supported terminal shows /dev/ttys003");

        let bad = tmux_os().fail("tmux switch-client", "switch failed");
        let e = focus(&bad.ports(), &b, "tmux:%1").unwrap_err();
        assert_eq!(e.to_string(), "switch failed");

        let e = focus(&os.ports(), &b, "tmux:%9").unwrap_err();
        assert!(e.to_string().contains("%9 not found"));

        let e = focus(&FakeOs::new(&l).ports(), &b, "tmux:%1").unwrap_err();
        assert_eq!(e.to_string(), "tmux is not running");
    }

    #[test]
    fn focus_tty_ref_tries_backends_in_order() {
        let l = log();
        let os = FakeOs::new(&l).answer("ps -axo", &ps_line(5, "ttys004"));
        let mut fakes = all_fake_backends(&l);
        fakes[3] = FakeBackend::new("kitty", &l).tty_error();
        let b: Vec<Box<dyn TerminalBackend>> = fakes.into_iter().map(FakeBackend::boxed).collect();
        let e = focus(&os.ports(), &b, "tty:ttys004").unwrap_err();
        assert_eq!(
            e.to_string(),
            "no supported terminal shows /dev/ttys004 (kitty: refused)"
        );
        assert_eq!(
            logged(&l, "focus_tty"),
            [
                "focus_tty:iterm2:ttys004",
                "focus_tty:terminal:ttys004",
                "focus_tty:kitty:ttys004",
                "focus_tty:wezterm:ttys004",
                "focus_tty:ghostty:ttys004"
            ]
        );

        l.borrow_mut().clear();
        let mut fakes = all_fake_backends(&l);
        fakes[4] = FakeBackend::new("wezterm", &l).shows("ttys004");
        let b: Vec<Box<dyn TerminalBackend>> = fakes.into_iter().map(FakeBackend::boxed).collect();
        focus(&os.ports(), &b, "tty:ttys004").unwrap();
        let tried = logged(&l, "focus_tty");
        assert_eq!(tried.last().unwrap(), "focus_tty:wezterm:ttys004");
        assert!(!tried.iter().any(|t| t.contains("ghostty")));
    }

    #[test]
    fn focus_tty_ref_for_gone_tty_errors() {
        let l = log();
        let os = FakeOs::new(&l).answer("ps -axo", &ps_line(5, "ttys001"));
        let b = vec![FakeBackend::new("iterm2", &l).shows("ttys009").boxed()];
        let e = focus(&os.ports(), &b, "tty:ttys009").unwrap_err();
        assert_eq!(
            e.to_string(),
            "the session in /dev/ttys009 is no longer running"
        );
        assert!(logged(&l, "focus_tty").is_empty());
        let failing = FakeOs::new(&l).fail("ps -axo", "ps failed");
        let e = focus(&failing.ports(), &b, "tty:ttys009").unwrap_err();
        assert_eq!(e.to_string(), "ps failed");
    }

    #[test]
    fn focus_unknown_prefix_errors() {
        let l = log();
        let os = FakeOs::new(&l);
        let b = vec![FakeBackend::new("iterm2", &l).boxed()];
        for r in ["foo:1", "noprefix", ""] {
            let e = focus(&os.ports(), &b, r).unwrap_err();
            assert!(e.to_string().starts_with("unknown tab ref"), "{r}: {e}");
        }
        assert!(l.borrow().is_empty());
    }

    #[test]
    fn focus_known_terminal_not_in_list_is_not_enabled() {
        let l = log();
        let os = FakeOs::new(&l);
        let b = vec![FakeBackend::new("iterm2", &l).boxed()];
        for id in ["ghostty", "terminal", "kitty", "wezterm"] {
            let e = focus(&os.ports(), &b, &format!("{id}:1")).unwrap_err();
            assert_eq!(e.to_string(), format!("{id} is not enabled"));
        }
        assert!(l.borrow().is_empty());
    }

    #[test]
    fn ghostty_hosted_tmux_pane_is_focusable() {
        // Ghostty reports no tty, so the pane is not nested under a tab, but
        // its session has a client, so the row keeps a tmux ref and focus
        // reaches Ghostty's focus_tty on the client tty.
        let l = log();
        let os = FakeOs::new(&l)
            .answer("ps -axo", &ps_line(10, "ttys101"))
            .answer(
                "tmux list-panes",
                "%4	g	3	1	/dev/ttys101	/w	p
",
            )
            .answer(
                "tmux list-clients",
                "/dev/ttys050	g
",
            )
            .answer("tmux switch-client", "")
            .answer("tmux select-window", "")
            .answer("tmux select-pane", "");
        let mk = || {
            vec![
                FakeBackend::new("ghostty", &l)
                    .tabs(vec![tab("ghostty", "ghostty:1", "✳ x", 1, 1)])
                    .shows("ttys050")
                    .boxed(),
                FakeBackend::new("iterm2", &l).boxed(),
            ]
        };
        let r = run_list(&os, mk(), vec![agent(10, None, Some(1))], &opts());
        assert_eq!(ids(&r.rows), ["tmux:%4", "ghostty:1"]);
        assert_eq!((r.rows[0].window, r.rows[0].index), (3, 1));
        assert_eq!(r.rows[0].agent.as_ref().unwrap().pid, 10);

        let os = os.answer("ps -axo", &ps_line(9, "ttys050"));
        l.borrow_mut().clear();
        focus(&os.ports(), &mk(), "tmux:%4").unwrap();
        assert_eq!(
            logged(&l, "focus_tty"),
            ["focus_tty:iterm2:ttys050", "focus_tty:ghostty:ttys050"]
        );
    }
}
