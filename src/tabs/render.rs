//! Rendering of tab rows: a human table, JSON, and Alfred script filter JSON.

use super::{AgentStatus, Row, TabsReport};
use serde::Serialize;

/// Longest title in the human table, in characters.
const HUMAN_TITLE_MAX: usize = 60;
/// Longest last prompt in an Alfred subtitle, in characters.
const PROMPT_MAX: usize = 80;

/// Collapse whitespace (newlines included) to single spaces and trim.
fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Cut `s` to at most `max` characters, ending with `…` when cut.
fn cut(s: &str, max: usize) -> String {
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
fn table(rows: &[[String; 5]]) -> String {
    let mut widths = [0usize; 5];
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

/// One line per row under a header, warnings first.
pub fn render_human(report: &TabsReport) -> String {
    let mut out: String = report
        .warnings
        .iter()
        .map(|w| format!("warning: {w}\n"))
        .collect();
    if report.rows.is_empty() {
        out.push_str("no matching tabs\n");
        return out;
    }
    let dash = |s: &str| {
        if s.is_empty() {
            "-".to_string()
        } else {
            s.to_string()
        }
    };
    let mut rows = vec![["TERMINAL", "TITLE", "CWD", "AGENT", "REF"].map(String::from)];
    for r in &report.rows {
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
    out.push_str(&table(&rows));
    out
}

/// `{"rows":[...],"warnings":[...]}`.
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
            mods: None,
        }
    }
}

#[derive(Serialize)]
struct Mods {
    cmd: Mod,
    #[serde(skip_serializing_if = "Option::is_none")]
    alt: Option<Mod>,
}

/// A modifier action: copy `arg`. Valid even when the row itself is not, so
/// rows without a ref can still be copied.
#[derive(Serialize)]
struct Mod {
    arg: String,
    subtitle: String,
    valid: bool,
}

impl Mod {
    fn copy(what: &str, arg: Option<&str>) -> Self {
        match arg {
            Some(a) => Mod {
                arg: a.to_string(),
                subtitle: format!("Copy {what}: {a}"),
                valid: true,
            },
            None => Mod {
                arg: String::new(),
                subtitle: "Nothing to copy".into(),
                valid: false,
            },
        }
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

fn subtitle(r: &Row, home: Option<&str>) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(a) = &r.agent {
        match a.status {
            AgentStatus::Busy => parts.push("● busy".into()),
            AgentStatus::Idle => parts.push("○ idle".into()),
            AgentStatus::Unknown => {}
        }
        parts.push(a.tool.clone());
    }
    parts.push(r.terminal.clone());
    parts.extend(cwd(r).map(|c| shorten_home(c, home)));
    let prompt = r
        .agent
        .as_ref()
        .and_then(|a| a.last_prompt.as_deref())
        .map(one_line)
        .filter(|p| !p.is_empty());
    parts.extend(prompt.map(|p| format!("“{}”", cut(&p, PROMPT_MAX))));
    parts.retain(|p| !p.is_empty());
    parts.join(" · ")
}

fn alfred_item(r: &Row, home: Option<&str>) -> AlfredItem {
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
        subtitle: subtitle(r, home),
        arg: r.ref_.clone(),
        valid: r.ref_.is_some(),
        mods: Some(Mods { cmd, alt }),
    }
}

/// Alfred script filter JSON, with the cwd shortened against `HOME`.
pub fn render_alfred(report: &TabsReport) -> String {
    render_alfred_with_home(report, std::env::var("HOME").ok().as_deref())
}

/// Alfred script filter JSON, with the cwd shortened against `home`.
///
/// Warnings come first as rows that cannot be actioned. With no rows, one
/// "No matching tabs" row follows.
pub fn render_alfred_with_home(report: &TabsReport, home: Option<&str>) -> String {
    let mut items: Vec<AlfredItem> = report
        .warnings
        .iter()
        .map(|w| AlfredItem::message(w))
        .collect();
    items.extend(report.rows.iter().map(|r| alfred_item(r, home)));
    if report.rows.is_empty() {
        items.push(AlfredItem::message("No matching tabs"));
    }
    serde_json::to_string(&AlfredOutput { items }).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tabs::{AgentInfo, AgentStatus, Row, Tab};
    use serde_json::Value;

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
        }
    }

    fn alfred(r: &TabsReport) -> Vec<Value> {
        let v: Value = serde_json::from_str(&render_alfred_with_home(r, Some("/Users/me")))
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
        let v: Value =
            serde_json::from_str(&render_alfred_with_home(&report(vec![unknown], &[]), None))
                .unwrap();
        let u = &v["items"][0];
        assert_eq!(u["subtitle"], "codex");
        assert_eq!(u["title"], "codex");
        let mut home_none = row("iterm2", Some("iterm2:b"), "", Some("/Users/me"));
        home_none.title = String::new();
        let v: Value = serde_json::from_str(&render_alfred_with_home(
            &report(vec![home_none], &[]),
            None,
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
    }
}
