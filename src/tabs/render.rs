//! Rendering of tab rows: a human table, JSON, and Alfred script filter JSON.

use super::{AgentStatus, ClosedSession, Row, TabsReport};
use serde::Serialize;

/// Longest title in the human table, in characters.
const HUMAN_TITLE_MAX: usize = 60;
/// Longest last prompt in an Alfred subtitle, in characters.
const PROMPT_MAX: usize = 80;

/// Collapse whitespace (newlines included) to single spaces and trim.
pub(crate) fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Cut `s` to at most `max` characters, ending with `…` when cut.
pub(crate) fn cut(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max - 1).collect();
    format!("{head}…")
}

/// The row's display title: the agent title, else the tab title, else the
/// agent tool.
fn title(r: &Row) -> &str {
    let agent_title = r.agent.as_ref().and_then(|a| a.title.as_deref());
    match agent_title.filter(|t| !t.is_empty()) {
        Some(t) => t,
        None if !r.title.is_empty() => &r.title,
        None => r.agent.as_ref().map_or("", |a| a.tool.as_str()),
    }
}

/// The row's cwd: the tab's, else the agent's.
fn cwd(r: &Row) -> Option<&str> {
    r.cwd
        .as_deref()
        .or_else(|| r.agent.as_ref().and_then(|a| a.cwd.as_deref()))
}

/// Left-aligned columns padded to the widest cell, two spaces apart.
fn table<const N: usize>(rows: &[[String; N]]) -> String {
    let mut widths = [0usize; N];
    for row in rows {
        for (w, cell) in widths.iter_mut().zip(row) {
            *w = (*w).max(cell.chars().count());
        }
    }
    let mut out = String::new();
    for row in rows {
        let cells: Vec<String> = row
            .iter()
            .zip(widths)
            .map(|(c, w)| format!("{c}{}", " ".repeat(w - c.chars().count())))
            .collect();
        out.push_str(cells.join("  ").trim_end());
        out.push('\n');
    }
    out
}

/// `s`, or `-` when it is empty.
fn dash(s: &str) -> String {
    if s.is_empty() {
        "-".to_string()
    } else {
        s.to_string()
    }
}

/// One line per row under a header, warnings first, then the closed
/// sessions when they were listed.
pub fn render_human(report: &TabsReport) -> String {
    let mut out: String = report
        .warnings
        .iter()
        .map(|w| format!("warning: {w}\n"))
        .collect();
    if report.rows.is_empty() {
        out.push_str("no matching tabs\n");
    } else {
        out.push_str(&rows_table(&report.rows));
    }
    if let Some(closed) = &report.closed {
        out.push_str("closed sessions\n");
        out.push_str(&closed_table(closed));
    }
    out
}

/// The tab rows under a header.
fn rows_table(report_rows: &[Row]) -> String {
    let mut rows = vec![["TERMINAL", "TITLE", "CWD", "AGENT", "REF"].map(String::from)];
    for r in report_rows {
        let agent = r.agent.as_ref().map_or(String::new(), |a| match a.status {
            AgentStatus::Busy => format!("{} busy", a.tool),
            AgentStatus::Idle => format!("{} idle", a.tool),
            AgentStatus::Unknown => a.tool.clone(),
        });
        rows.push([
            dash(&r.terminal),
            dash(&cut(&one_line(title(r)), HUMAN_TITLE_MAX)),
            dash(cwd(r).unwrap_or("")),
            dash(&agent),
            dash(r.ref_.as_deref().unwrap_or("")),
        ]);
    }
    table(&rows)
}

/// One line per closed session: tool, title and folder.
fn closed_table(closed: &[ClosedSession]) -> String {
    if closed.is_empty() {
        return "no matching closed sessions\n".into();
    }
    let rows: Vec<[String; 3]> = closed
        .iter()
        .map(|c| {
            let title = c.title.as_deref().map(one_line).unwrap_or_default();
            [
                c.tool.clone(),
                dash(&cut(&title, HUMAN_TITLE_MAX)),
                c.cwd.clone(),
            ]
        })
        .collect();
    table(&rows)
}

/// `{"rows":[...],"warnings":[...]}`, plus `"closed":[...]` when listed.
pub fn render_json(report: &TabsReport) -> String {
    serde_json::to_string_pretty(report).unwrap_or_default()
}

#[derive(Serialize)]
struct AlfredOutput {
    items: Vec<AlfredItem>,
}

#[derive(Serialize)]
struct AlfredItem {
    // No `uid`: Alfred would re-rank rows by what the user picked, and csx's
    // order (agents by recent activity) must stay.
    title: String,
    subtitle: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    arg: Option<String>,
    valid: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    variables: Option<Vars>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mods: Option<Mods>,
}

impl AlfredItem {
    /// A row that only shows a message.
    fn message(title: &str) -> Self {
        AlfredItem {
            title: title.to_string(),
            subtitle: String::new(),
            arg: None,
            valid: false,
            variables: None,
            mods: None,
        }
    }
}

/// Workflow variables of an item. The workflow routes on `csx_kind`: `tab`
/// focuses, `closed` opens in the saved terminal (or asks), `closed_pick`
/// asks for a terminal for this one session.
#[derive(Serialize)]
struct Vars {
    csx_kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    csx_tool: Option<String>,
}

impl Vars {
    fn tab() -> Self {
        Vars {
            csx_kind: "tab",
            csx_tool: None,
        }
    }

    fn closed(kind: &'static str, tool: &str) -> Self {
        Vars {
            csx_kind: kind,
            csx_tool: Some(tool.to_string()),
        }
    }
}

#[derive(Serialize)]
struct Mods {
    cmd: Mod,
    #[serde(skip_serializing_if = "Option::is_none")]
    alt: Option<Mod>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ctrl: Option<Mod>,
}

/// A modifier action: copy `arg`. Valid even when the row itself is not, so
/// rows without a ref can still be copied.
#[derive(Serialize)]
struct Mod {
    arg: String,
    subtitle: String,
    valid: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    variables: Option<Vars>,
}

impl Mod {
    fn copy(what: &str, arg: Option<&str>) -> Self {
        match arg {
            Some(a) => Mod {
                arg: a.to_string(),
                subtitle: format!("Copy {what}: {a}"),
                valid: true,
                variables: None,
            },
            None => Mod {
                arg: String::new(),
                subtitle: "Nothing to copy".into(),
                valid: false,
                variables: None,
            },
        }
    }
}

/// How long ago `ts_ms` was, as of `now_ms` (both unix milliseconds):
/// `just now`, `5m ago`, `3h ago`, `2d ago`, `4mo ago` or `1y ago`. A time
/// in the future (clock skew) reads `just now`.
fn ago(ts_ms: i64, now_ms: i64) -> String {
    const MIN: i64 = 60;
    const HOUR: i64 = 60 * MIN;
    const DAY: i64 = 24 * HOUR;
    let secs = now_ms.saturating_sub(ts_ms).max(0) / 1000;
    match secs {
        s if s < MIN => "just now".into(),
        s if s < HOUR => format!("{}m ago", s / MIN),
        s if s < DAY => format!("{}h ago", s / HOUR),
        s if s < 30 * DAY => format!("{}d ago", s / DAY),
        s if s < 365 * DAY => format!("{}mo ago", s / (30 * DAY)),
        s => format!("{}y ago", s / (365 * DAY)),
    }
}

/// `path` with `home` (when given and non-empty) shortened to `~`.
fn shorten_home(path: &str, home: Option<&str>) -> String {
    let Some(home) = home.filter(|h| !h.is_empty()) else {
        return path.to_string();
    };
    match path.strip_prefix(home) {
        Some("") => "~".into(),
        Some(rest) if rest.starts_with('/') => format!("~{rest}"),
        _ => path.to_string(),
    }
}

fn subtitle(r: &Row, home: Option<&str>, now_ms: i64) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(a) = &r.agent {
        match a.status {
            AgentStatus::Busy => parts.push("● busy".into()),
            AgentStatus::Idle => parts.push("○ idle".into()),
            AgentStatus::Unknown => {}
        }
        parts.extend(a.updated_at.map(|t| ago(t, now_ms)));
        parts.push(a.tool.clone());
    }
    parts.push(r.terminal.clone());
    parts.extend(cwd(r).map(|c| shorten_home(c, home)));
    parts.extend(prompt_part(
        r.agent.as_ref().and_then(|a| a.last_prompt.as_deref()),
    ));
    parts.retain(|p| !p.is_empty());
    parts.join(" · ")
}

fn alfred_item(r: &Row, home: Option<&str>, now_ms: i64) -> AlfredItem {
    let session = r.agent.as_ref().and_then(|a| a.session_id.as_deref());
    let cmd = match session {
        Some(id) => Mod::copy("session id", Some(id)),
        None => Mod::copy("working directory", cwd(r)),
    };
    let alt = r
        .agent
        .as_ref()
        .and_then(|a| a.resume_command.as_deref())
        .map(|c| Mod::copy("resume command", Some(c)));
    AlfredItem {
        title: title(r).to_string(),
        subtitle: subtitle(r, home, now_ms),
        arg: r.ref_.clone(),
        valid: r.ref_.is_some(),
        variables: Some(Vars::tab()),
        mods: Some(Mods {
            cmd,
            alt,
            ctrl: None,
        }),
    }
}

/// The last prompt as a quoted subtitle part, cut to [`PROMPT_MAX`].
fn prompt_part(prompt: Option<&str>) -> Option<String> {
    prompt
        .map(one_line)
        .filter(|p| !p.is_empty())
        .map(|p| format!("“{}”", cut(&p, PROMPT_MAX)))
}

/// A closed session: Return opens it in the saved terminal, Ctrl+Return
/// picks a terminal for this session, Cmd and Option copy.
fn closed_item(c: &ClosedSession, home: Option<&str>, now_ms: i64) -> AlfredItem {
    let title = c.title.as_deref().map(one_line).unwrap_or_default();
    let title = if title.is_empty() {
        c.session_id.clone()
    } else {
        title
    };
    let mut parts = vec!["closed".to_string()];
    parts.extend(c.updated_at.map(|t| ago(t, now_ms)));
    parts.push(c.tool.clone());
    parts.push(shorten_home(&c.cwd, home));
    parts.extend(prompt_part(c.last_prompt.as_deref()));
    let resume = Some(c.resume_command.as_str()).filter(|r| !r.is_empty());
    AlfredItem {
        title,
        subtitle: parts.join(" · "),
        arg: Some(c.session_id.clone()),
        valid: true,
        variables: Some(Vars::closed("closed", &c.tool)),
        mods: Some(Mods {
            cmd: Mod::copy("session id", Some(&c.session_id)),
            alt: Some(Mod::copy("resume command", resume)),
            ctrl: Some(Mod {
                arg: c.session_id.clone(),
                subtitle: "Choose a terminal for this session".into(),
                valid: true,
                variables: Some(Vars::closed("closed_pick", &c.tool)),
            }),
        }),
    }
}

/// Alfred script filter JSON, with the cwd shortened against `HOME` and
/// last activity shown relative to the system clock.
pub fn render_alfred(report: &TabsReport) -> String {
    render_alfred_with_home(report, std::env::var("HOME").ok().as_deref(), now_ms())
}

/// The system clock in unix milliseconds, or 0 before 1970.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

/// Alfred script filter JSON, with the cwd shortened against `home` and
/// last activity shown relative to `now_ms` (unix milliseconds).
///
/// Warnings come first as rows that cannot be actioned, then the open rows,
/// then the closed sessions when they were listed. With neither rows nor
/// closed sessions, one "No matching tabs" row follows the warnings.
pub fn render_alfred_with_home(report: &TabsReport, home: Option<&str>, now_ms: i64) -> String {
    let mut items: Vec<AlfredItem> = report
        .warnings
        .iter()
        .map(|w| AlfredItem::message(w))
        .collect();
    items.extend(report.rows.iter().map(|r| alfred_item(r, home, now_ms)));
    let closed = report.closed.as_deref().unwrap_or_default();
    if report.rows.is_empty() && closed.is_empty() {
        items.push(AlfredItem::message("No matching tabs"));
    }
    items.extend(closed.iter().map(|c| closed_item(c, home, now_ms)));
    serde_json::to_string(&AlfredOutput { items }).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tabs::{AgentInfo, AgentStatus, ClosedSession, Row, Tab};
    use serde_json::Value;

    /// The clock the Alfred tests render against.
    const NOW: i64 = 1_800_000_000_000;

    fn row(terminal: &str, ref_: Option<&str>, title: &str, cwd: Option<&str>) -> Row {
        Tab {
            terminal: terminal.into(),
            ref_: ref_.map(str::to_string),
            title: title.into(),
            cwd: cwd.map(str::to_string),
            window: 1,
            index: 1,
            ..Tab::default()
        }
    }

    fn report(rows: Vec<Row>, warnings: &[&str]) -> TabsReport {
        TabsReport {
            rows,
            warnings: warnings.iter().map(|w| w.to_string()).collect(),
            ..TabsReport::default()
        }
    }

    fn closed(tool: &str, id: &str, title: Option<&str>, cwd: &str) -> ClosedSession {
        ClosedSession {
            tool: tool.into(),
            session_id: id.into(),
            title: title.map(str::to_string),
            cwd: cwd.into(),
            ..ClosedSession::default()
        }
    }

    #[test]
    fn render_human_lists_closed_sessions_after_the_rows() {
        let mut r = report(
            vec![row("iterm2", Some("iterm2:a"), "zsh", Some("/x"))],
            &[],
        );
        r.closed = Some(vec![
            closed(
                "claude-code",
                "u1",
                Some("Fix\nlogin"),
                "/Users/me/work/acme",
            ),
            closed("devin", "swift-otter", None, "/srv/app"),
        ]);
        let out = render_human(&r);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 5);
        assert!(lines[1].starts_with("iterm2"));
        assert_eq!(lines[2], "closed sessions");
        assert_eq!(lines[3], "claude-code  Fix login  /Users/me/work/acme");
        assert_eq!(lines[4], "devin        -          /srv/app");

        // No rows: the closed list still prints.
        let mut r = report(vec![], &[]);
        r.closed = Some(vec![closed("codex", "u2", Some("t"), "/w")]);
        assert_eq!(
            render_human(&r),
            "no matching tabs\nclosed sessions\ncodex  t  /w\n"
        );
        r.closed = Some(vec![]);
        assert_eq!(
            render_human(&r),
            "no matching tabs\nclosed sessions\nno matching closed sessions\n"
        );
    }

    fn full_closed(tool: &str, id: &str, title: Option<&str>, cwd: &str) -> ClosedSession {
        ClosedSession {
            last_prompt: Some(format!("next\n{}", "p".repeat(90))),
            resume_command: format!("cd '{cwd}' && {tool} --resume {id}"),
            ..closed(tool, id, title, cwd)
        }
    }

    #[test]
    fn render_alfred_closed_items_follow_the_open_rows() {
        let mut open = row("ghostty", Some("ghostty:1"), "zsh", Some("/Users/me/w"));
        open.agent = Some(AgentInfo {
            tool: "claude-code".into(),
            session_id: Some("live".into()),
            ..AgentInfo::default()
        });
        let mut r = report(vec![open, row("iterm2", None, "x", None)], &["kitty: off"]);
        r.closed = Some(vec![
            full_closed(
                "claude-code",
                "u1",
                Some("Fix\nlogin"),
                "/Users/me/work/acme",
            ),
            closed("devin", "swift-otter", None, "/srv/app"),
            closed("codex", "u3", Some(" \n "), "/Users/me"),
        ]);
        let items = alfred(&r);
        assert_eq!(items.len(), 6);
        assert_eq!(items[0]["title"], "kitty: off");
        assert!(items[0].get("variables").is_none());
        // Every open row says it is a tab, also one with no ref.
        for i in [1, 2] {
            assert_eq!(
                items[i]["variables"],
                serde_json::json!({"csx_kind": "tab"})
            );
            assert!(items[i]["mods"].get("ctrl").is_none());
        }
        assert_eq!(items[1]["arg"], "ghostty:1");

        let c = &items[3];
        assert!(c.get("uid").is_none());
        assert_eq!(c["title"], "Fix login");
        let prompt = format!("next {}…", "p".repeat(74));
        assert_eq!(
            c["subtitle"],
            format!("closed · claude-code · ~/work/acme · “{prompt}”")
        );
        assert_eq!(c["arg"], "u1");
        assert_eq!(c["valid"], true);
        assert_eq!(
            c["variables"],
            serde_json::json!({"csx_kind": "closed", "csx_tool": "claude-code"})
        );
        assert_eq!(
            c["mods"]["cmd"],
            serde_json::json!({"arg": "u1", "subtitle": "Copy session id: u1", "valid": true})
        );
        let resume = "cd '/Users/me/work/acme' && claude-code --resume u1";
        assert_eq!(
            c["mods"]["alt"],
            serde_json::json!({
                "arg": resume,
                "subtitle": format!("Copy resume command: {resume}"),
                "valid": true
            })
        );
        assert_eq!(
            c["mods"]["ctrl"],
            serde_json::json!({
                "arg": "u1",
                "subtitle": "Choose a terminal for this session",
                "valid": true,
                "variables": {"csx_kind": "closed_pick", "csx_tool": "claude-code"}
            })
        );

        // No title: the session id. No prompt: no quote part.
        let d = &items[4];
        assert_eq!(d["title"], "swift-otter");
        assert_eq!(d["subtitle"], "closed · devin · /srv/app");
        assert_eq!(d["variables"]["csx_tool"], "devin");
        assert_eq!(d["mods"]["ctrl"]["variables"]["csx_tool"], "devin");
        assert_eq!(d["mods"]["alt"]["arg"], "");
        assert_eq!(d["mods"]["alt"]["valid"], false);
        let e = &items[5];
        assert_eq!(e["title"], "u3");
        assert_eq!(e["subtitle"], "closed · codex · ~");
    }

    #[test]
    fn render_alfred_no_matching_tabs_only_when_both_lists_are_empty() {
        let mut r = report(vec![], &[]);
        r.closed = Some(vec![closed("codex", "u2", Some("t"), "/w")]);
        let items = alfred(&r);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["title"], "t");
        assert_eq!(items[0]["variables"]["csx_kind"], "closed");
        r.closed = Some(vec![]);
        let items = alfred(&r);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["title"], "No matching tabs");
        assert!(items[0].get("variables").is_none());
        r.closed = None;
        assert_eq!(alfred(&r)[0]["title"], "No matching tabs");
    }

    #[test]
    fn render_alfred_closed_items_with_hostile_text() {
        let hostile = "He said \"hi\" \\ </script>&' 🚀";
        let mut c = full_closed("codex", "s\"1\\", Some(hostile), "/w/\"q\"");
        c.last_prompt = Some("say \"x\"\\n\n🙂".into());
        let mut r = report(vec![], &[]);
        r.closed = Some(vec![c]);
        let raw = render_alfred_with_home(&r, Some("/Users/me"), NOW);
        let items = alfred(&r);
        assert_eq!(items[0]["title"], hostile);
        assert_eq!(items[0]["arg"], "s\"1\\");
        assert_eq!(
            items[0]["subtitle"],
            "closed · codex · /w/\"q\" · “say \"x\"\\n 🙂”"
        );
        assert_eq!(items[0]["mods"]["ctrl"]["arg"], "s\"1\\");
        assert!(!raw.contains('\n'));
    }

    #[test]
    fn ago_reads_each_unit() {
        let s = 1000;
        let cases = [
            (-5 * s, "just now"),
            (0, "just now"),
            (59 * s, "just now"),
            (60 * s, "1m ago"),
            (59 * 60 * s, "59m ago"),
            (3600 * s, "1h ago"),
            (23 * 3600 * s, "23h ago"),
            (86_400 * s, "1d ago"),
            (29 * 86_400 * s, "29d ago"),
            (30 * 86_400 * s, "1mo ago"),
            (364 * 86_400 * s, "12mo ago"),
            (365 * 86_400 * s, "1y ago"),
            (800 * 86_400 * s, "2y ago"),
        ];
        for (age, want) in cases {
            assert_eq!(ago(NOW - age, NOW), want, "age {age} ms");
        }
        assert_eq!(
            ago(i64::MIN, i64::MAX),
            format!("{}y ago", i64::MAX / 1000 / (365 * 86_400))
        );
    }

    #[test]
    fn render_alfred_shows_last_activity() {
        let mut open = row("ghostty", Some("ghostty:1"), "zsh", Some("/Users/me/w"));
        open.agent = Some(AgentInfo {
            tool: "claude-code".into(),
            status: AgentStatus::Idle,
            updated_at: Some(NOW - 5 * 60_000),
            ..AgentInfo::default()
        });
        let mut r = report(vec![open], &[]);
        r.closed = Some(vec![ClosedSession {
            updated_at: Some(NOW - 2 * 86_400_000),
            ..closed("codex", "u1", Some("t"), "/Users/me/x")
        }]);
        let items = alfred(&r);
        assert_eq!(
            items[0]["subtitle"],
            "○ idle · 5m ago · claude-code · ghostty · ~/w"
        );
        assert_eq!(items[1]["subtitle"], "closed · 2d ago · codex · ~/x");
    }

    fn alfred(r: &TabsReport) -> Vec<Value> {
        let v: Value = serde_json::from_str(&render_alfred_with_home(r, Some("/Users/me"), NOW))
            .expect("valid json");
        v["items"].as_array().unwrap().clone()
    }

    #[test]
    fn render_alfred_valid_json_with_hostile_titles() {
        let hostile = [
            "He said \"hi\"",
            "back\\slash",
            "line1\nline2",
            "🚀 ship it",
            "</script>&'",
        ];
        let mut rows: Vec<Row> = hostile
            .iter()
            .map(|t| row("ghostty", Some("ghostty:1"), t, Some("/w/\"q\"")))
            .collect();
        rows[0].agent = Some(AgentInfo {
            tool: "claude-code".into(),
            title: Some("A \"quoted\"\\ title\n🚀".into()),
            last_prompt: Some("say \"x\"\\n\n🙂".into()),
            session_id: Some("s\"1".into()),
            ..AgentInfo::default()
        });
        let items = alfred(&report(rows, &["bad \"warning\"\n"]));
        assert_eq!(items[0]["title"], "bad \"warning\"\n");
        assert_eq!(items[1]["title"], "A \"quoted\"\\ title\n🚀");
        assert_eq!(items[1]["mods"]["cmd"]["arg"], "s\"1");
        for (i, t) in hostile.iter().enumerate().skip(1) {
            assert_eq!(items[i + 1]["title"], *t);
        }
        let plain: Value = serde_json::from_str(&render_alfred(&report(vec![], &[]))).unwrap();
        assert_eq!(plain["items"][0]["title"], "No matching tabs");
    }

    #[test]
    fn render_alfred_mods() {
        let prompt = format!("first line\n{}", "x".repeat(100));
        let mut busy = row("ghostty", Some("ghostty:7"), "✳ tab", Some("/Users/me/w"));
        busy.agent = Some(AgentInfo {
            tool: "claude-code".into(),
            pid: 9,
            title: Some("Fix parser".into()),
            last_prompt: Some(prompt),
            session_id: Some("s1".into()),
            status: AgentStatus::Busy,
            resume_command: Some("claude --resume s1".into()),
            ..AgentInfo::default()
        });
        let tab = row("iterm2", Some("iterm2:a"), "shell", Some("/Users/me"));
        let mut idle = row("", None, "", Some("/Users/meow"));
        idle.agent = Some(AgentInfo {
            tool: "gemini".into(),
            pid: 42,
            status: AgentStatus::Idle,
            last_prompt: Some("  \n ".into()),
            ..AgentInfo::default()
        });
        let bare = row("tmux", None, "pane", None);
        let items = alfred(&report(vec![busy, tab, idle, bare], &[]));

        let b = &items[0];
        assert!(b.get("uid").is_none());
        assert_eq!(b["arg"], "ghostty:7");
        assert_eq!(b["valid"], true);
        assert_eq!(b["title"], "Fix parser");
        let cut = format!("first line {}…", "x".repeat(68));
        assert_eq!(
            b["subtitle"],
            format!("● busy · claude-code · ghostty · ~/w · “{cut}”")
        );
        assert_eq!(b["mods"]["cmd"]["arg"], "s1");
        assert_eq!(b["mods"]["cmd"]["valid"], true);
        assert_eq!(b["mods"]["alt"]["arg"], "claude --resume s1");
        assert_eq!(b["mods"]["alt"]["valid"], true);

        let t = &items[1];
        assert_eq!(t["title"], "shell");
        assert_eq!(t["subtitle"], "iterm2 · ~");
        assert_eq!(t["mods"]["cmd"]["arg"], "/Users/me");
        assert!(t["mods"].get("alt").is_none());

        let i = &items[2];
        assert!(i.get("arg").is_none());
        assert_eq!(i["valid"], false);
        assert_eq!(i["title"], "gemini");
        assert_eq!(i["subtitle"], "○ idle · gemini · /Users/meow");
        assert_eq!(i["mods"]["cmd"]["arg"], "/Users/meow");
        assert_eq!(i["mods"]["cmd"]["valid"], true);

        let n = &items[3];
        assert!(n.get("uid").is_none());
        assert_eq!(n["valid"], false);
        assert_eq!(n["subtitle"], "tmux");
        assert_eq!(n["mods"]["cmd"]["valid"], false);

        let mut unknown = row("", None, "", None);
        unknown.agent = Some(AgentInfo {
            tool: "codex".into(),
            ..AgentInfo::default()
        });
        let v: Value = serde_json::from_str(&render_alfred_with_home(
            &report(vec![unknown], &[]),
            None,
            NOW,
        ))
        .unwrap();
        let u = &v["items"][0];
        assert_eq!(u["subtitle"], "codex");
        assert_eq!(u["title"], "codex");
        let mut home_none = row("iterm2", Some("iterm2:b"), "", Some("/Users/me"));
        home_none.title = String::new();
        let v: Value = serde_json::from_str(&render_alfred_with_home(
            &report(vec![home_none], &[]),
            None,
            NOW,
        ))
        .unwrap();
        assert_eq!(v["items"][0]["subtitle"], "iterm2 · /Users/me");
        assert_eq!(v["items"][0]["title"], "");
    }

    #[test]
    fn render_alfred_warning_and_empty_rows() {
        let items = alfred(&report(vec![], &["ghostty: boom"]));
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["title"], "ghostty: boom");
        assert_eq!(items[0]["valid"], false);
        assert!(items[0].get("uid").is_none());
        assert_eq!(items[1]["title"], "No matching tabs");
        assert_eq!(items[1]["valid"], false);

        let rows = vec![row("iterm2", Some("iterm2:a"), "s", None)];
        let items = alfred(&report(rows, &["kitty: off"]));
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["title"], "kitty: off");
        assert_eq!(items[1]["arg"], "iterm2:a");
        assert!(items.iter().all(|i| i.get("uid").is_none()));
    }

    #[test]
    fn render_human_columns() {
        let mut a = row("ghostty", Some("ghostty:1"), "✳ tab", Some("/w"));
        a.agent = Some(AgentInfo {
            tool: "claude-code".into(),
            title: Some("Fix\nparser".into()),
            status: AgentStatus::Busy,
            ..AgentInfo::default()
        });
        let mut lone = row("", None, "", None);
        lone.agent = Some(AgentInfo {
            tool: "codex".into(),
            ..AgentInfo::default()
        });
        let long = "t".repeat(70);
        let t = row("iterm2", Some("iterm2:a"), &long, Some("/x"));
        let out = render_human(&report(vec![a, lone, t], &["kitty: off"]));
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "warning: kitty: off");
        let cols = |l: &str| -> Vec<String> {
            l.split("  ")
                .filter(|c| !c.is_empty())
                .map(|c| c.trim().to_string())
                .collect()
        };
        assert_eq!(cols(lines[1]), ["TERMINAL", "TITLE", "CWD", "AGENT", "REF"]);
        assert_eq!(
            cols(lines[2]),
            [
                "ghostty",
                "Fix parser",
                "/w",
                "claude-code busy",
                "ghostty:1"
            ]
        );
        assert_eq!(cols(lines[3]), ["-", "codex", "-", "codex", "-"]);
        let trunc = format!("{}…", "t".repeat(59));
        assert_eq!(
            cols(lines[4]),
            ["iterm2", trunc.as_str(), "/x", "-", "iterm2:a"]
        );
        assert_eq!(lines[1].find("TITLE"), lines[2].find("Fix parser"));
        assert_eq!(lines.len(), 5);

        let mut idle = row("kitty", Some("kitty:1"), "k", None);
        idle.agent = Some(AgentInfo {
            tool: "aider".into(),
            status: AgentStatus::Idle,
            ..AgentInfo::default()
        });
        assert!(render_human(&report(vec![idle], &[])).contains("aider idle"));
        assert_eq!(render_human(&report(vec![], &[])), "no matching tabs\n");
    }

    #[test]
    fn render_json_shape() {
        let mut a = row("ghostty", Some("ghostty:1"), "t", None);
        a.agent = Some(AgentInfo {
            tool: "claude-code".into(),
            status: AgentStatus::Idle,
            ..AgentInfo::default()
        });
        let v: Value = serde_json::from_str(&render_json(&report(vec![a], &["w"]))).unwrap();
        assert_eq!(v["rows"][0]["ref"], "ghostty:1");
        assert_eq!(v["rows"][0]["terminal"], "ghostty");
        assert_eq!(v["rows"][0]["agent"]["status"], "idle");
        assert_eq!(v["warnings"], serde_json::json!(["w"]));
        let empty: Value = serde_json::from_str(&render_json(&TabsReport::default())).unwrap();
        assert_eq!(empty, serde_json::json!({"rows": [], "warnings": []}));
        // Live session ids stay out of the JSON; `closed` shows only when listed.
        let live = TabsReport {
            live_sessions: vec!["s1".into()],
            closed: Some(vec![]),
            ..TabsReport::default()
        };
        let v: Value = serde_json::from_str(&render_json(&live)).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"rows": [], "warnings": [], "closed": []})
        );
    }

    /// The Raycast extension parses this same file, so a renamed or removed
    /// field fails here before it breaks the extension.
    #[test]
    fn render_json_matches_raycast_fixture() {
        let s = |v: &str| Some(v.to_string());
        let claude = Tab {
            terminal: "ghostty".into(),
            ref_: s("ghostty:AB-1"),
            title: "✳ fix login".into(),
            cwd: s("/Users/me/work/acme"),
            tty: None,
            window: 1,
            index: 3,
            agent: Some(AgentInfo {
                tool: "claude-code".into(),
                pid: 42,
                tty: s("ttys001"),
                cwd: s("/Users/me/work/acme"),
                session_id: s("0f1e2d3c"),
                title: s("Fix login"),
                last_prompt: s("run the tests"),
                status: AgentStatus::Busy,
                updated_at: Some(1_793_156_134_829),
                resume_command: s("cd '/Users/me/work/acme' && claude --resume 0f1e2d3c"),
            }),
        };
        let plain = Tab {
            terminal: "iterm2".into(),
            ref_: s("iterm2:w0t1p0"),
            title: "zsh".into(),
            tty: s("ttys004"),
            window: 2,
            index: 1,
            ..Tab::default()
        };
        let tabless = Tab {
            ref_: s("tty:ttys009"),
            cwd: s("/srv/app"),
            tty: s("ttys009"),
            agent: Some(AgentInfo {
                tool: "gemini".into(),
                pid: 7,
                tty: s("ttys009"),
                cwd: s("/srv/app"),
                ..AgentInfo::default()
            }),
            ..Tab::default()
        };
        let mut r = report(
            vec![claude, plain, tabless],
            &["kitty: remote control is off"],
        );
        r.closed = Some(vec![ClosedSession {
            tool: "claude-code".into(),
            session_id: "4a5b6c7d-0000-4000-8000-00000000beef".into(),
            title: s("Add dark mode"),
            cwd: "/Users/me/work/acme-web".into(),
            last_prompt: s("make the toggle persist"),
            updated_at: Some(1_793_150_000_000),
            resume_command:
                "cd '/Users/me/work/acme-web' && claude --resume 4a5b6c7d-0000-4000-8000-00000000beef"
                    .into(),
        }]);
        let got: Value = serde_json::from_str(&render_json(&r)).unwrap();
        let want: Value = serde_json::from_str(include_str!(
            "../../contrib/raycast/csx-tabs/test/fixtures/report.json"
        ))
        .unwrap();
        assert_eq!(got, want);
    }
}
