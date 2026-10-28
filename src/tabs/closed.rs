//! Closed sessions: indexed sessions of the resumable tools that no detected
//! agent runs, newest first, with a title, last prompt and resume command.

use std::path::Path;

use super::app::{process_list_failed, ListOptions};
use super::render::{cut, one_line};
use super::{words_match, ClosedSession, TabsReport};
use crate::agent::claude::{read_tail, scan_transcript, slug, TranscriptInfo};
use crate::agent::{resume_line, session_env, RESUMABLE_TOOLS};
use crate::db::{Db, ResumableSession};
use crate::error::Result;

/// Longest title taken from a first prompt, in characters.
const TITLE_CHARS: usize = 80;

/// What closed sessions to list.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClosedOptions {
    /// Most sessions to return, counted after the query match.
    pub limit: usize,
    /// Query words; every word must match a session.
    pub query: Vec<String>,
}

/// Closed sessions, newest first: indexed sessions of [`RESUMABLE_TOOLS`]
/// whose id is not in `live`, whose folder is absolute and still exists,
/// whose folders can be typed safely, and whose id passes the tool's check. Titles and prompts are read only for
/// sessions that pass those checks. `home` decides which config folders are the defaults.
pub fn list_closed(
    db: &Db,
    live: &[String],
    opts: &ClosedOptions,
    home: &Path,
) -> Result<Vec<ClosedSession>> {
    let mut out = Vec::new();
    for s in db.resumable_sessions(RESUMABLE_TOOLS)? {
        if out.len() >= opts.limit {
            break;
        }
        if live.contains(&s.session_id) {
            continue;
        }
        let Some(cwd) = s.project_path.as_deref() else {
            continue;
        };
        if !Path::new(cwd).is_absolute() {
            continue;
        }
        let env = session_env(&s.tool, s.config_dir.as_deref(), home);
        let Some(resume_command) = resume_line(&s.tool, &s.session_id, cwd, &env) else {
            continue;
        };
        if !Path::new(cwd).is_dir() {
            continue;
        }
        let entry = entry(db, &s, cwd, resume_command, home)?;
        if matches_closed(&entry, &opts.query) {
            out.push(entry);
        }
    }
    Ok(out)
}

/// Build the entry for a session that passed the cheap checks. Title: the
/// Claude transcript's custom title, else its AI title, else the first user
/// prompt, else the project name. Last prompt: the transcript's, else the
/// index's.
fn entry(
    db: &Db,
    s: &ResumableSession,
    cwd: &str,
    resume_command: String,
    home: &Path,
) -> Result<ClosedSession> {
    let tr = if s.tool == "claude-code" {
        claude_transcript(s, cwd, home)
    } else {
        TranscriptInfo::default()
    };
    let title = tr.custom_title.or(tr.ai_title);
    let (first, last) = if title.is_none() || tr.last_prompt.is_none() {
        db.user_prompts(&s.session_id)?
    } else {
        (None, None)
    };
    Ok(ClosedSession {
        tool: s.tool.clone(),
        session_id: s.session_id.clone(),
        title: title
            .or_else(|| {
                first
                    .map(|p| cut(&one_line(&p), TITLE_CHARS))
                    .filter(|t| !t.is_empty())
            })
            .or_else(|| s.project_name.clone()),
        cwd: cwd.to_string(),
        last_prompt: tr.last_prompt.or(last),
        updated_at: s.last_ts.map(|t| t * 1000),
        resume_command,
    })
}

/// Titles and last prompt from the tail of a Claude transcript:
/// `<config dir>/projects/<slug(cwd)>/<id>.jsonl`, with `<home>/.claude` when
/// the source has no config folder.
fn claude_transcript(s: &ResumableSession, cwd: &str, home: &Path) -> TranscriptInfo {
    let dir = s
        .config_dir
        .as_deref()
        .map_or_else(|| home.join(".claude"), |d| Path::new(d).to_path_buf());
    let path = dir
        .join("projects")
        .join(slug(cwd))
        .join(format!("{}.jsonl", s.session_id));
    read_tail(&path)
        .map(|t| scan_transcript(&t))
        .unwrap_or_default()
}

/// True when every query word (compared case-insensitively) appears in the
/// title, folder, last prompt, tool or session id. An empty query matches.
fn matches_closed(c: &ClosedSession, words: &[String]) -> bool {
    let mut parts: Vec<&str> = vec![&c.cwd, &c.tool, &c.session_id];
    parts.extend(c.title.as_deref());
    parts.extend(c.last_prompt.as_deref());
    words_match(&parts, words)
}

/// When `opts.closed` asks for closed sessions, list them into
/// `report.closed`, leaving out the sessions in `report.live_sessions` and
/// matching `opts.query`. A failure becomes a warning and an empty list, so
/// the open tabs still show. When the process list could not be read, no
/// agent was detected and running sessions would look closed, so the list
/// stays empty and a warning says why.
pub fn add_closed(report: &mut TabsReport, db: &Db, opts: &ListOptions, home: &Path) {
    let Some(limit) = opts.closed else {
        return;
    };
    if process_list_failed(report) {
        report
            .warnings
            .push("closed sessions hidden: could not read the process list".into());
        report.closed = Some(Vec::new());
        return;
    }
    let closed_opts = ClosedOptions {
        limit,
        query: opts.query.clone(),
    };
    match list_closed(db, &report.live_sessions, &closed_opts, home) {
        Ok(closed) => report.closed = Some(closed),
        Err(e) => {
            report.warnings.push(format!("closed sessions: {e}"));
            report.closed = Some(Vec::new());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::claude::slug;
    use crate::db::{Db, SourceRow};
    use crate::model::{MessageRecord, Role, SessionMeta, Tool};
    use crate::tabs::app::ListOptions;
    use crate::tabs::TabsReport;
    use std::fs;
    use std::path::{Path, PathBuf};

    const U1: &str = "11111111-1111-4111-8111-111111111111";
    const U2: &str = "22222222-2222-4222-8222-222222222222";
    const U3: &str = "33333333-3333-4333-8333-333333333333";

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("csx-closed-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// A project folder `name` under `root`, created.
    fn folder(root: &Path, name: &str) -> String {
        let p = root.join(name);
        fs::create_dir_all(&p).unwrap();
        p.to_str().unwrap().to_string()
    }

    fn source(db: &Db, tool: &str, config_dir: Option<&str>) -> i64 {
        db.upsert_source(&SourceRow {
            tool: Some(tool.into()),
            config_dir: config_dir.map(str::to_string),
            profile: Some(format!("{tool}-{config_dir:?}")),
            ..SourceRow::default()
        })
        .unwrap()
    }

    fn session(
        db: &Db,
        src: i64,
        tool: Tool,
        id: &str,
        cwd: Option<&str>,
        name: Option<&str>,
        last_ts: i64,
    ) {
        let meta = SessionMeta {
            session_id: id.into(),
            tool,
            project_path: cwd.map(str::to_string),
            repo_id: None,
            project_name: name.map(str::to_string),
            git_branch: None,
            account: None,
            first_ts: 1,
            last_ts,
        };
        db.upsert_session(src, &meta, 0, Some("summary is not a title"))
            .unwrap();
    }

    fn user(db: &Db, src: i64, tool: Tool, id: &str, seq: u64, text: &str) {
        let m = MessageRecord {
            session_id: id.into(),
            tool,
            seq,
            ts: 1,
            role: Role::User,
            tool_name: None,
            uuid: None,
            text: text.into(),
            cwd: None,
        };
        db.insert_message(src, &m).unwrap();
    }

    fn opts(limit: usize, query: &[&str]) -> ClosedOptions {
        ClosedOptions {
            limit,
            query: query.iter().map(|q| q.to_string()).collect(),
        }
    }

    fn ids(v: &[ClosedSession]) -> Vec<&str> {
        v.iter().map(|c| c.session_id.as_str()).collect()
    }

    #[test]
    fn skips_live_missing_folder_invalid_id_and_no_path() {
        let root = tmp("skips");
        let home = root.join("home");
        let db = Db::open(":memory:").unwrap();
        let codex = source(&db, "codex", None);
        let devin = source(&db, "devin", None);
        let live = folder(&root, "live");
        let gone = folder(&root, "gone");
        let kept = folder(&root, "kept");
        fs::remove_dir_all(&gone).unwrap();
        session(&db, codex, Tool::Codex, U1, Some(&live), None, 60);
        session(&db, codex, Tool::Codex, U2, Some(&gone), None, 50);
        session(&db, devin, Tool::Devin, "Bad Id", Some(&kept), None, 40);
        session(&db, codex, Tool::Codex, "no-path", None, None, 30);
        session(&db, devin, Tool::Devin, "quiet-fox", Some(&kept), None, 20);
        session(&db, codex, Tool::Codex, U3, Some(&kept), Some("kept"), 10);
        let got = list_closed(&db, &[U1.to_string()], &opts(50, &[]), &home).unwrap();
        assert_eq!(ids(&got), ["quiet-fox", U3]);
        assert_eq!(got[0].tool, "devin");
        assert_eq!(
            got[0].resume_command,
            format!("cd '{kept}' && devin -r quiet-fox")
        );
        assert_eq!(got[1].updated_at, Some(10_000));
        assert_eq!(got[1].cwd, kept);
    }

    #[test]
    fn limit_applies_after_query_match() {
        let root = tmp("limit");
        let db = Db::open(":memory:").unwrap();
        let codex = source(&db, "codex", None);
        let a = folder(&root, "alpha");
        let n = folder(&root, "needle");
        session(&db, codex, Tool::Codex, U1, Some(&a), None, 30);
        session(&db, codex, Tool::Codex, U2, Some(&n), None, 20);
        session(&db, codex, Tool::Codex, U3, Some(&a), None, 10);
        let home = root.join("home");
        let newest = list_closed(&db, &[], &opts(1, &[]), &home).unwrap();
        assert_eq!(ids(&newest), [U1]);
        let found = list_closed(&db, &[], &opts(1, &["NEEDLE"]), &home).unwrap();
        assert_eq!(ids(&found), [U2]);
        assert!(list_closed(&db, &[], &opts(0, &[]), &home)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn query_matches_every_field_and_needs_every_word() {
        let root = tmp("query");
        let db = Db::open(":memory:").unwrap();
        let codex = source(&db, "codex", None);
        let p = folder(&root, "acme");
        session(&db, codex, Tool::Codex, U1, Some(&p), Some("acme"), 10);
        user(&db, codex, Tool::Codex, U1, 0, "Fix the login page");
        user(&db, codex, Tool::Codex, U1, 1, "now add a test");
        let home = root.join("home");
        let hit = |q: &[&str]| {
            !list_closed(&db, &[], &opts(5, q), &home)
                .unwrap()
                .is_empty()
        };
        assert!(hit(&["login"]));
        assert!(hit(&["ADD", "test"]));
        assert!(hit(&["acme"]));
        assert!(hit(&["codex"]));
        assert!(hit(&["1111-4111"]));
        assert!(hit(&[]));
        assert!(!hit(&["login", "missing"]));
    }

    #[test]
    fn claude_title_and_prompt_come_from_the_transcript() {
        let root = tmp("claude");
        let home = root.join("home");
        let config = root.join("cfg");
        let db = Db::open(":memory:").unwrap();
        let src = source(&db, "claude-code", config.to_str());
        let p = folder(&root, "web");
        let transcript = |id: &str, body: &str| {
            let path = config
                .join("projects")
                .join(slug(&p))
                .join(format!("{id}.jsonl"));
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, body).unwrap();
        };
        transcript(
            U1,
            concat!(
                "{\"type\":\"ai-title\",\"aiTitle\":\"ai one\"}\n",
                "{\"type\":\"custom-title\",\"customTitle\":\"Dark mode\"}\n",
                "{\"type\":\"last-prompt\",\"lastPrompt\":\"make it persist\"}\n",
            ),
        );
        transcript(
            U2,
            "{\"type\":\"ai-title\",\"aiTitle\":\"Ship the parser\"}\n",
        );
        session(&db, src, Tool::ClaudeCode, U1, Some(&p), Some("web"), 20);
        session(&db, src, Tool::ClaudeCode, U2, Some(&p), Some("web"), 10);
        user(&db, src, Tool::ClaudeCode, U1, 0, "first typed prompt");
        user(&db, src, Tool::ClaudeCode, U2, 0, "first typed prompt");
        user(&db, src, Tool::ClaudeCode, U2, 1, "last typed prompt");
        let got = list_closed(&db, &[], &opts(5, &[]), &home).unwrap();
        assert_eq!(got[0].title.as_deref(), Some("Dark mode"));
        assert_eq!(got[0].last_prompt.as_deref(), Some("make it persist"));
        assert_eq!(got[1].title.as_deref(), Some("Ship the parser"));
        // No last prompt in the transcript: the index has one.
        assert_eq!(got[1].last_prompt.as_deref(), Some("last typed prompt"));
    }

    #[test]
    fn title_falls_back_to_first_prompt_then_project_name() {
        let root = tmp("fallback");
        let home = root.join("home");
        let db = Db::open(":memory:").unwrap();
        // A Claude source with no config dir reads `<home>/.claude`, which
        // holds no transcript here.
        let claude = source(&db, "claude-code", None);
        let codex = source(&db, "codex", None);
        let p = folder(&root, "acme");
        let long = format!("Fix  the\nlogin {}", "x".repeat(100));
        session(
            &db,
            claude,
            Tool::ClaudeCode,
            U1,
            Some(&p),
            Some("acme"),
            40,
        );
        user(&db, claude, Tool::ClaudeCode, U1, 0, &long);
        session(&db, codex, Tool::Codex, U2, Some(&p), Some("acme"), 30);
        session(&db, codex, Tool::Codex, U3, Some(&p), None, 20);
        let got = list_closed(&db, &[], &opts(5, &[]), &home).unwrap();
        let want = format!("Fix the login {}…", "x".repeat(65));
        assert_eq!(got[0].title.as_deref(), Some(want.as_str()));
        assert_eq!(got[0].last_prompt.as_deref(), Some(long.as_str()));
        assert_eq!(got[1].title.as_deref(), Some("acme"));
        assert_eq!(got[1].last_prompt, None);
        assert_eq!(got[2].title, None);
    }

    #[test]
    fn env_prefix_only_for_a_non_default_config_dir() {
        let root = tmp("env");
        let home = root.join("home");
        let default = home.join(".claude");
        let other = root.join(".claude-b");
        let db = Db::open(":memory:").unwrap();
        let a = source(&db, "claude-code", default.to_str());
        let b = source(&db, "claude-code", other.to_str());
        let p = folder(&root, "acme");
        session(&db, a, Tool::ClaudeCode, U1, Some(&p), None, 20);
        session(&db, b, Tool::ClaudeCode, U2, Some(&p), None, 10);
        let got = list_closed(&db, &[], &opts(5, &[]), &home).unwrap();
        assert_eq!(
            got[0].resume_command,
            format!("cd '{p}' && claude --resume {U1}")
        );
        assert_eq!(
            got[1].resume_command,
            format!(
                "cd '{p}' && CLAUDE_CONFIG_DIR='{}' claude --resume {U2}",
                other.display()
            )
        );
    }

    #[test]
    fn add_closed_fills_the_report_or_warns() {
        let root = tmp("add");
        let home = root.join("home");
        let db = Db::open(":memory:").unwrap();
        let codex = source(&db, "codex", None);
        let p = folder(&root, "acme");
        session(&db, codex, Tool::Codex, U1, Some(&p), None, 20);
        session(&db, codex, Tool::Codex, U2, Some(&p), None, 10);
        let mut report = TabsReport {
            live_sessions: vec![U1.into()],
            ..TabsReport::default()
        };
        add_closed(&mut report, &db, &ListOptions::default(), &home);
        assert_eq!(report.closed, None);
        let with = ListOptions {
            closed: Some(5),
            query: vec!["acme".into()],
            ..ListOptions::default()
        };
        add_closed(&mut report, &db, &with, &home);
        assert_eq!(ids(report.closed.as_deref().unwrap()), [U2]);
        assert!(report.warnings.is_empty());

        db.conn().execute_batch("DROP TABLE sessions;").unwrap();
        let mut report = TabsReport::default();
        add_closed(&mut report, &db, &with, &home);
        assert_eq!(report.closed, Some(vec![]));
        assert_eq!(report.warnings.len(), 1);
        assert!(report.warnings[0].starts_with("closed sessions: "));
    }

    #[test]
    fn skips_relative_project_paths() {
        let root = tmp("relative");
        let db = Db::open(":memory:").unwrap();
        let codex = source(&db, "codex", None);
        let kept = folder(&root, "kept");
        // `src` exists relative to the test's working folder, so only the
        // absolute check can drop it. `-rf` would read as an option.
        session(&db, codex, Tool::Codex, U1, Some("src"), None, 30);
        session(&db, codex, Tool::Codex, U2, Some("-rf"), None, 20);
        session(&db, codex, Tool::Codex, U3, Some(&kept), None, 10);
        let got = list_closed(&db, &[], &opts(5, &[]), &root).unwrap();
        assert_eq!(ids(&got), [U3]);
    }

    #[test]
    fn skips_folders_and_config_dirs_that_cannot_be_typed_safely() {
        let root = tmp("unsafe");
        let db = Db::open(":memory:").unwrap();
        let codex = source(&db, "codex", None);
        let odd = source(&db, "codex", Some("/c/x\\y"));
        let bad = folder(&root, "a\x03b");
        let kept = folder(&root, "it's $x `y`");
        session(&db, codex, Tool::Codex, U1, Some(&bad), None, 30);
        session(&db, odd, Tool::Codex, U2, Some(&kept), None, 20);
        session(&db, codex, Tool::Codex, U3, Some(&kept), None, 10);
        let got = list_closed(&db, &[], &opts(5, &[]), &root).unwrap();
        assert_eq!(ids(&got), [U3]);
    }

    #[test]
    fn a_first_prompt_that_is_only_whitespace_is_not_a_title() {
        let root = tmp("blank-title");
        let db = Db::open(":memory:").unwrap();
        let codex = source(&db, "codex", None);
        let p = folder(&root, "acme");
        session(&db, codex, Tool::Codex, U1, Some(&p), Some("acme"), 10);
        // SQL trim keeps these characters; `one_line` drops them.
        user(&db, codex, Tool::Codex, U1, 0, "\u{a0}\u{b}\u{2003}");
        let got = list_closed(&db, &[], &opts(5, &[]), &root).unwrap();
        assert_eq!(got[0].title.as_deref(), Some("acme"));
    }

    #[test]
    fn add_closed_hides_the_list_when_the_process_list_failed() {
        let root = tmp("add-ps");
        let db = Db::open(":memory:").unwrap();
        let codex = source(&db, "codex", None);
        let p = folder(&root, "acme");
        session(&db, codex, Tool::Codex, U1, Some(&p), None, 20);
        let mut report = TabsReport {
            warnings: vec!["ps: ps failed: boom".into()],
            ..TabsReport::default()
        };
        let with = ListOptions {
            closed: Some(5),
            ..ListOptions::default()
        };
        add_closed(&mut report, &db, &with, &root);
        assert_eq!(report.closed, Some(vec![]));
        assert_eq!(
            report.warnings,
            [
                "ps: ps failed: boom",
                "closed sessions hidden: could not read the process list"
            ]
        );
        // Without the closed list asked for, nothing is added.
        let mut report = TabsReport {
            warnings: vec!["ps: ps failed: boom".into()],
            ..TabsReport::default()
        };
        add_closed(&mut report, &db, &ListOptions::default(), &root);
        assert_eq!((report.closed, report.warnings.len()), (None, 1));
    }

    #[test]
    fn prompt_errors_propagate() {
        let root = tmp("err");
        let db = Db::open(":memory:").unwrap();
        let codex = source(&db, "codex", None);
        let p = folder(&root, "acme");
        session(&db, codex, Tool::Codex, U1, Some(&p), None, 20);
        db.conn().execute_batch("DROP TABLE messages;").unwrap();
        assert!(list_closed(&db, &[], &opts(5, &[]), &root).is_err());
    }
}
