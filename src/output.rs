//! Pure rendering of query results.
//!
//! Every function here is a total transformation from data to a `String`: it
//! performs no I/O and no process exit, so it is exhaustively unit-testable.
//! [`crate::cli`] calls these and prints the result; the formatting decisions
//! all live here.

use serde_json::json;

use crate::db::{CurrentAccount, Hit, MessageRow, SessionRow, SourceSummary};

/// The two rendering modes every command supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// A human-readable aligned table.
    Human,
    /// A machine-readable JSON document.
    Json,
}

impl Format {
    /// Choose a format from the `--json` flag.
    pub fn from_json_flag(json: bool) -> Format {
        if json {
            Format::Json
        } else {
            Format::Human
        }
    }
}

/// Render a unix timestamp as a compact UTC `YYYY-MM-DD HH:MM:SS` string.
///
/// A small dependency-free civil-date conversion (days-since-epoch to Y/M/D via
/// the standard algorithm) so output is stable and testable without pulling in
/// a datetime crate.
pub fn fmt_ts(ts: i64) -> String {
    let days = ts.div_euclid(86_400);
    let secs = ts.rem_euclid(86_400);
    let (hh, mm, ss) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}:{ss:02}")
}

/// Convert a count of days since the Unix epoch into a `(year, month, day)`
/// civil date (Howard Hinnant's `civil_from_days`).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// Collapse internal whitespace (including newlines) to single spaces and trim,
/// so a body renders on one table line.
fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Truncate `s` to at most `max` characters, appending an ellipsis when cut.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{head}…")
}

/// Render `"-"` for an absent value, else the value.
fn or_dash(v: Option<&str>) -> &str {
    v.unwrap_or("-")
}

/// Render a table given a header and rows of equal-length string cells. Columns
/// are left-aligned and padded to the widest cell (by character count). An
/// empty `rows` still prints the header.
fn table(header: &[&str], rows: &[Vec<String>]) -> String {
    let cols = header.len();
    let mut widths: Vec<usize> = header.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate().take(cols) {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }
    let mut out = String::new();
    let render_row = |out: &mut String, cells: &[&str]| {
        for (i, cell) in cells.iter().enumerate().take(cols) {
            if i > 0 {
                out.push_str("  ");
            }
            out.push_str(cell);
            // Pad all but the last column.
            if i + 1 < cols {
                let pad = widths[i].saturating_sub(cell.chars().count());
                for _ in 0..pad {
                    out.push(' ');
                }
            }
        }
        out.push('\n');
    };
    render_row(&mut out, header);
    for row in rows {
        let refs: Vec<&str> = row.iter().map(String::as_str).collect();
        render_row(&mut out, &refs);
    }
    out
}

/// Render search hits in the selected format.
pub fn render_hits(hits: &[Hit], format: Format) -> String {
    match format {
        Format::Json => {
            let arr: Vec<_> = hits
                .iter()
                .map(|h| {
                    json!({
                        "session_id": h.session_id,
                        "tool": h.tool,
                        "repo_id": h.repo_id,
                        "project_name": h.project_name,
                        "ts": h.ts,
                        "snippet": h.snippet,
                        "score": h.score,
                    })
                })
                .collect();
            serde_json::to_string_pretty(&arr).unwrap_or_else(|_| "[]".into())
        }
        Format::Human => {
            if hits.is_empty() {
                return "no matches".to_string();
            }
            let rows: Vec<Vec<String>> = hits
                .iter()
                .map(|h| {
                    vec![
                        format!("{:.2}", h.score),
                        fmt_ts(h.ts),
                        or_dash(h.tool.as_deref()).to_string(),
                        or_dash(h.project_name.as_deref()).to_string(),
                        truncate(&one_line(&h.snippet), 60),
                        truncate(&h.session_id, 12),
                    ]
                })
                .collect();
            table(
                &["SCORE", "WHEN", "TOOL", "PROJECT", "SNIPPET", "SESSION"],
                &rows,
            )
        }
    }
}

/// Render source summaries in the selected format.
pub fn render_sources(sources: &[SourceSummary], format: Format) -> String {
    match format {
        Format::Json => {
            let arr: Vec<_> = sources
                .iter()
                .map(|s| {
                    json!({
                        "id": s.id,
                        "tool": s.tool,
                        "email": s.email,
                        "org": s.org,
                        "profile": s.profile,
                        "sessions": s.sessions,
                        "messages": s.messages,
                    })
                })
                .collect();
            serde_json::to_string_pretty(&arr).unwrap_or_else(|_| "[]".into())
        }
        Format::Human => {
            if sources.is_empty() {
                return "no sources indexed".to_string();
            }
            let rows: Vec<Vec<String>> = sources
                .iter()
                .map(|s| {
                    vec![
                        or_dash(s.tool.as_deref()).to_string(),
                        or_dash(s.email.as_deref()).to_string(),
                        or_dash(s.org.as_deref()).to_string(),
                        or_dash(s.profile.as_deref()).to_string(),
                        s.sessions.to_string(),
                        s.messages.to_string(),
                    ]
                })
                .collect();
            table(
                &["TOOL", "EMAIL", "ORG", "PROFILE", "SESSIONS", "MESSAGES"],
                &rows,
            )
        }
    }
}

/// Render a session list in the selected format.
pub fn render_sessions(sessions: &[SessionRow], format: Format) -> String {
    match format {
        Format::Json => {
            let arr: Vec<_> = sessions
                .iter()
                .map(|s| {
                    json!({
                        "session_id": s.session_id,
                        "tool": s.tool,
                        "repo_id": s.repo_id,
                        "project_name": s.project_name,
                        "git_branch": s.git_branch,
                        "first_ts": s.first_ts,
                        "last_ts": s.last_ts,
                        "msg_count": s.msg_count,
                    })
                })
                .collect();
            serde_json::to_string_pretty(&arr).unwrap_or_else(|_| "[]".into())
        }
        Format::Human => {
            if sessions.is_empty() {
                return "no sessions".to_string();
            }
            let rows: Vec<Vec<String>> = sessions
                .iter()
                .map(|s| {
                    vec![
                        fmt_ts(s.last_ts),
                        or_dash(s.tool.as_deref()).to_string(),
                        or_dash(s.project_name.as_deref()).to_string(),
                        or_dash(s.git_branch.as_deref()).to_string(),
                        s.msg_count.to_string(),
                        truncate(&s.session_id, 24),
                    ]
                })
                .collect();
            table(
                &["LAST", "TOOL", "PROJECT", "BRANCH", "MSGS", "SESSION"],
                &rows,
            )
        }
    }
}

/// Render a single session's messages (the `show` command).
pub fn render_messages(session_id: &str, msgs: &[MessageRow], format: Format) -> String {
    match format {
        Format::Json => {
            let arr: Vec<_> = msgs
                .iter()
                .map(|m| {
                    json!({
                        "seq": m.seq,
                        "ts": m.ts,
                        "role": m.role,
                        "tool_name": m.tool_name,
                        "body": m.body,
                    })
                })
                .collect();
            let doc = json!({ "session_id": session_id, "messages": arr });
            serde_json::to_string_pretty(&doc).unwrap_or_else(|_| "{}".into())
        }
        Format::Human => {
            if msgs.is_empty() {
                return format!("session {session_id}: no messages");
            }
            let mut out = format!("session {session_id}\n");
            for m in msgs {
                let who = match m.tool_name.as_deref() {
                    Some(tn) => format!("{}({tn})", or_dash(m.role.as_deref())),
                    None => or_dash(m.role.as_deref()).to_string(),
                };
                out.push_str(&format!("\n[{}] {} {}\n", m.seq, fmt_ts(m.ts), who));
                out.push_str(m.body.trim_end());
                out.push('\n');
            }
            out
        }
    }
}

/// Render the active-account-per-tool summary (the `current` command).
pub fn render_current(accounts: &[CurrentAccount], format: Format) -> String {
    match format {
        Format::Json => {
            let arr: Vec<_> = accounts
                .iter()
                .map(|a| {
                    json!({
                        "tool": a.tool,
                        "email": a.email,
                        "org": a.org,
                        "profile": a.profile,
                        "last_ts": a.last_ts,
                    })
                })
                .collect();
            serde_json::to_string_pretty(&arr).unwrap_or_else(|_| "[]".into())
        }
        Format::Human => {
            if accounts.is_empty() {
                return "no active accounts".to_string();
            }
            let rows: Vec<Vec<String>> = accounts
                .iter()
                .map(|a| {
                    vec![
                        a.tool.clone(),
                        or_dash(a.email.as_deref()).to_string(),
                        or_dash(a.org.as_deref()).to_string(),
                        or_dash(a.profile.as_deref()).to_string(),
                        fmt_ts(a.last_ts),
                    ]
                })
                .collect();
            table(&["TOOL", "EMAIL", "ORG", "PROFILE", "LAST"], &rows)
        }
    }
}

/// Render an `ask` answer: the model's text plus its cited sessions.
pub fn render_answer(answer: &str, citations: &[String], format: Format) -> String {
    match format {
        Format::Json => {
            let doc = json!({ "answer": answer, "citations": citations });
            serde_json::to_string_pretty(&doc).unwrap_or_else(|_| "{}".into())
        }
        Format::Human => {
            let mut out = answer.trim_end().to_string();
            if !citations.is_empty() {
                out.push_str("\n\nsources:");
                for c in citations {
                    out.push_str(&format!("\n  - {c}"));
                }
            }
            out
        }
    }
}

/// Render a completed sync pass's [`crate::index::SyncStats`] as a one-line
/// human summary or a JSON object.
pub fn render_sync_stats(stats: &crate::index::SyncStats, format: Format) -> String {
    match format {
        Format::Json => {
            let doc = json!({
                "files_seen": stats.files_seen,
                "files_skipped": stats.files_skipped,
                "files_indexed": stats.files_indexed,
                "messages_added": stats.messages_added,
                "sessions_touched": stats.sessions_touched,
            });
            serde_json::to_string_pretty(&doc).unwrap_or_else(|_| "{}".into())
        }
        Format::Human => format!(
            "sync: {} files seen, {} indexed, {} skipped, {} messages added, {} sessions touched",
            stats.files_seen,
            stats.files_indexed,
            stats.files_skipped,
            stats.messages_added,
            stats.sessions_touched,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::SyncStats;

    fn hit(session: &str, tool: &str, project: &str, ts: i64, snippet: &str, score: f64) -> Hit {
        Hit {
            session_id: session.into(),
            tool: Some(tool.into()),
            repo_id: Some("repo".into()),
            project_name: Some(project.into()),
            ts,
            snippet: snippet.into(),
            score,
        }
    }

    #[test]
    fn format_from_flag() {
        assert_eq!(Format::from_json_flag(true), Format::Json);
        assert_eq!(Format::from_json_flag(false), Format::Human);
    }

    #[test]
    fn fmt_ts_known_epochs() {
        assert_eq!(fmt_ts(0), "1970-01-01 00:00:00");
        // 2021-01-01 00:00:00 UTC.
        assert_eq!(fmt_ts(1_609_459_200), "2021-01-01 00:00:00");
        // A leap-year date: 2020-02-29 12:34:56 UTC.
        assert_eq!(fmt_ts(1_582_979_696), "2020-02-29 12:34:56");
        // Pre-epoch (negative) still converts correctly.
        assert_eq!(fmt_ts(-86_400), "1969-12-31 00:00:00");
    }

    #[test]
    fn truncate_and_one_line_helpers() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("abcdefgh", 4), "abc…");
        assert_eq!(one_line("a\n  b\t c"), "a b c");
        assert_eq!(or_dash(None), "-");
        assert_eq!(or_dash(Some("x")), "x");
    }

    #[test]
    fn table_pads_and_prints_header_when_empty() {
        let t = table(&["A", "BB"], &[]);
        assert_eq!(t, "A  BB\n");
        let t = table(
            &["A", "B"],
            &[
                vec!["longcell".into(), "x".into()],
                vec!["y".into(), "z".into()],
            ],
        );
        let lines: Vec<&str> = t.lines().collect();
        assert_eq!(lines[0], "A         B");
        assert_eq!(lines[1], "longcell  x");
        assert_eq!(lines[2], "y         z");
    }

    #[test]
    fn render_hits_human_and_json() {
        let hits = vec![
            hit(
                "sess-abc",
                "claude-code",
                "projx",
                0,
                "the [parser] bug",
                3.5,
            ),
            hit("sess-def", "codex", "projy", 86_400, "slow [parser]", 1.25),
        ];
        let human = render_hits(&hits, Format::Human);
        assert!(human.contains("SCORE"));
        assert!(human.contains("3.50"));
        assert!(human.contains("[parser]"));
        assert!(human.contains("1970-01-01"));
        // Session id truncated to 12 chars.
        assert!(human.contains("sess-abc"));

        let js = render_hits(&hits, Format::Json);
        let parsed: serde_json::Value = serde_json::from_str(&js).unwrap();
        assert_eq!(parsed.as_array().unwrap().len(), 2);
        assert_eq!(parsed[0]["session_id"], "sess-abc");
        assert_eq!(parsed[0]["tool"], "claude-code");
        assert_eq!(parsed[0]["score"], 3.5);
        assert_eq!(parsed[1]["ts"], 86_400);
    }

    #[test]
    fn render_hits_empty() {
        assert_eq!(render_hits(&[], Format::Human), "no matches");
        assert_eq!(render_hits(&[], Format::Json), "[]");
    }

    #[test]
    fn render_sources_human_and_json() {
        let sources = vec![
            SourceSummary {
                id: 1,
                tool: Some("claude-code".into()),
                email: Some("a@example.com".into()),
                org: Some("Acme".into()),
                profile: Some("work".into()),
                sessions: 2,
                messages: 40,
            },
            SourceSummary {
                id: 2,
                tool: Some("codex".into()),
                email: None,
                org: None,
                profile: None,
                sessions: 1,
                messages: 7,
            },
        ];
        let human = render_sources(&sources, Format::Human);
        assert!(human.contains("TOOL"));
        assert!(human.contains("a@example.com"));
        assert!(human.contains("Acme"));
        // Missing fields render as a dash.
        assert!(human.contains("codex"));
        assert!(human
            .lines()
            .any(|l| l.contains("codex") && l.contains('-')));

        let js = render_sources(&sources, Format::Json);
        let parsed: serde_json::Value = serde_json::from_str(&js).unwrap();
        assert_eq!(parsed[0]["messages"], 40);
        assert_eq!(parsed[1]["email"], serde_json::Value::Null);
    }

    #[test]
    fn render_sources_empty() {
        assert_eq!(render_sources(&[], Format::Human), "no sources indexed");
        assert_eq!(render_sources(&[], Format::Json), "[]");
    }

    #[test]
    fn render_sessions_human_and_json() {
        let sessions = vec![SessionRow {
            session_id: "s-1".into(),
            tool: Some("codex".into()),
            repo_id: Some("repo-y".into()),
            project_name: Some("projy".into()),
            git_branch: Some("feature".into()),
            first_ts: 300,
            last_ts: 400,
            msg_count: 12,
        }];
        let human = render_sessions(&sessions, Format::Human);
        assert!(human.contains("SESSION"));
        assert!(human.contains("projy"));
        assert!(human.contains("feature"));
        assert!(human.contains("12"));

        let js = render_sessions(&sessions, Format::Json);
        let parsed: serde_json::Value = serde_json::from_str(&js).unwrap();
        assert_eq!(parsed[0]["session_id"], "s-1");
        assert_eq!(parsed[0]["msg_count"], 12);
        assert_eq!(parsed[0]["git_branch"], "feature");
    }

    #[test]
    fn render_sessions_empty() {
        assert_eq!(render_sessions(&[], Format::Human), "no sessions");
        assert_eq!(render_sessions(&[], Format::Json), "[]");
    }

    #[test]
    fn render_messages_human_and_json() {
        let msgs = vec![
            MessageRow {
                seq: 0,
                ts: 0,
                role: Some("user".into()),
                tool_name: None,
                body: "how do I fix it".into(),
            },
            MessageRow {
                seq: 1,
                ts: 60,
                role: Some("tool".into()),
                tool_name: Some("grep".into()),
                body: "matched\n".into(),
            },
        ];
        let human = render_messages("s-1", &msgs, Format::Human);
        assert!(human.contains("session s-1"));
        assert!(human.contains("[0]"));
        assert!(human.contains("how do I fix it"));
        // Tool name is shown alongside the role.
        assert!(human.contains("tool(grep)"));

        let js = render_messages("s-1", &msgs, Format::Json);
        let parsed: serde_json::Value = serde_json::from_str(&js).unwrap();
        assert_eq!(parsed["session_id"], "s-1");
        assert_eq!(parsed["messages"].as_array().unwrap().len(), 2);
        assert_eq!(parsed["messages"][1]["tool_name"], "grep");
    }

    #[test]
    fn render_messages_empty() {
        assert_eq!(
            render_messages("s-x", &[], Format::Human),
            "session s-x: no messages"
        );
        let js = render_messages("s-x", &[], Format::Json);
        let parsed: serde_json::Value = serde_json::from_str(&js).unwrap();
        assert_eq!(parsed["session_id"], "s-x");
        assert_eq!(parsed["messages"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn render_current_human_and_json() {
        let accts = vec![CurrentAccount {
            tool: "claude-code".into(),
            email: Some("a@example.com".into()),
            org: Some("Acme".into()),
            profile: Some("work".into()),
            last_ts: 200,
        }];
        let human = render_current(&accts, Format::Human);
        assert!(human.contains("TOOL"));
        assert!(human.contains("claude-code"));
        assert!(human.contains("a@example.com"));

        let js = render_current(&accts, Format::Json);
        let parsed: serde_json::Value = serde_json::from_str(&js).unwrap();
        assert_eq!(parsed[0]["tool"], "claude-code");
        assert_eq!(parsed[0]["last_ts"], 200);
    }

    #[test]
    fn render_current_empty() {
        assert_eq!(render_current(&[], Format::Human), "no active accounts");
        assert_eq!(render_current(&[], Format::Json), "[]");
    }

    #[test]
    fn render_answer_human_and_json() {
        let human = render_answer(
            "the fix is X\n",
            &["s-1".into(), "s-2".into()],
            Format::Human,
        );
        assert!(human.contains("the fix is X"));
        assert!(human.contains("sources:"));
        assert!(human.contains("- s-1"));

        let no_cites = render_answer("plain answer", &[], Format::Human);
        assert_eq!(no_cites, "plain answer");

        let js = render_answer("ans", &["s-1".into()], Format::Json);
        let parsed: serde_json::Value = serde_json::from_str(&js).unwrap();
        assert_eq!(parsed["answer"], "ans");
        assert_eq!(parsed["citations"][0], "s-1");
    }

    #[test]
    fn render_sync_stats_human_and_json() {
        let stats = SyncStats {
            files_seen: 5,
            files_skipped: 1,
            files_indexed: 4,
            messages_added: 100,
            sessions_touched: 3,
        };
        let human = render_sync_stats(&stats, Format::Human);
        assert!(human.contains("5 files seen"));
        assert!(human.contains("100 messages added"));

        let js = render_sync_stats(&stats, Format::Json);
        let parsed: serde_json::Value = serde_json::from_str(&js).unwrap();
        assert_eq!(parsed["files_indexed"], 4);
        assert_eq!(parsed["sessions_touched"], 3);
    }
}
