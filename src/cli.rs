//! Command-line interface.
//!
//! Parsing lives in the [`Cli`]/[`Command`] clap-derive types. Each command is
//! handled by a small function that takes its dependencies (a [`Db`], the
//! configured sources, a [`GitRunner`], an [`AskEngine`]) and a `Write` sink by
//! reference, so every handler is unit-testable with an in-memory database and
//! a `Vec<u8>` buffer. The only impure surface is [`run`], which resolves the
//! real environment (config dirs, on-disk db) and writes to stdout; it is a
//! thin wrapper the tests bypass.

use std::io::Write;

use clap::{Parser, Subcommand};

use crate::db::{Db, Scope, SearchOpts};
use crate::error::{Error, Result};
use crate::git_shim::GitRunner;
use crate::index::sync;
use crate::output::{
    render_answer, render_current, render_hits, render_messages, render_sessions, render_sources,
    render_sync_stats, Format,
};
use crate::source::SessionSource;

/// Top-level CLI parser.
#[derive(Debug, Parser)]
#[command(
    name = "csx",
    version,
    about = "Local search over AI-coding session transcripts"
)]
pub struct Cli {
    /// Subcommand to run; defaults to a short help/version banner.
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// Scope filter flags shared by scoped commands. Each maps onto a field of
/// [`Scope`]; all are optional and ANDed together.
#[derive(Debug, Default, Clone, clap::Args)]
pub struct ScopeArgs {
    /// Restrict to a source account UUID.
    #[arg(long)]
    pub account: Option<String>,
    /// Restrict to a source organization UUID.
    #[arg(long)]
    pub org: Option<String>,
    /// Restrict to a named profile.
    #[arg(long)]
    pub profile: Option<String>,
    /// Restrict to a tool (`claude-code` or `codex`).
    #[arg(long)]
    pub tool: Option<String>,
    /// Restrict to a repository id.
    #[arg(long)]
    pub repo: Option<String>,
    /// Restrict to a session working directory / project path.
    #[arg(long)]
    pub cwd: Option<String>,
    /// Restrict to a git branch.
    #[arg(long)]
    pub branch: Option<String>,
    /// Restrict to a single session id.
    #[arg(long)]
    pub session: Option<String>,
    /// Restrict to a message role (`user`, `assistant`, `tool`, `system`).
    #[arg(long)]
    pub role: Option<String>,
    /// Restrict to a message tool-call name.
    #[arg(long = "tool-call")]
    pub tool_call: Option<String>,
    /// Lower bound on message timestamp, unix seconds (inclusive).
    #[arg(long)]
    pub since: Option<i64>,
    /// Upper bound on message timestamp, unix seconds (inclusive).
    #[arg(long)]
    pub until: Option<i64>,
}

impl ScopeArgs {
    /// Lower the flags into a storage [`Scope`].
    pub fn to_scope(&self) -> Scope {
        Scope {
            account_uuid: self.account.clone(),
            org_uuid: self.org.clone(),
            profile: self.profile.clone(),
            tool: self.tool.clone(),
            repo_id: self.repo.clone(),
            cwd: self.cwd.clone(),
            branch: self.branch.clone(),
            session_id: self.session.clone(),
            role: self.role.clone(),
            tool_name: self.tool_call.clone(),
            since: self.since,
            until: self.until,
        }
    }
}

/// Top-level subcommands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the indexer over the configured sources.
    Sync {
        /// Emit the sync stats as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Full-text / hybrid search over indexed transcripts.
    Query {
        /// The query text (FTS or, with `--code`, trigram substring).
        text: String,
        /// Scope filters.
        #[command(flatten)]
        scope: ScopeArgs,
        /// Maximum number of hits.
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Use the trigram (substring / code) index.
        #[arg(long)]
        code: bool,
        /// Emit hits as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Summarize indexed sources (accounts / tools).
    List {
        /// Emit as JSON.
        #[arg(long)]
        json: bool,
    },
    /// List sessions, optionally scoped.
    Sessions {
        /// Scope filters.
        #[command(flatten)]
        scope: ScopeArgs,
        /// Maximum number of sessions.
        #[arg(long, default_value_t = 50)]
        limit: usize,
        /// Emit as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Print a single session's messages.
    Show {
        /// The session id.
        id: String,
        /// Emit as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Show the active account per tool.
    Current {
        /// Emit as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Ask a natural-language question (retrieval-augmented answer).
    Ask {
        /// The question text.
        question: String,
        /// Scope filters applied to retrieval.
        #[command(flatten)]
        scope: ScopeArgs,
        /// Maximum number of retrieved passages to ground the answer.
        #[arg(long, default_value_t = 8)]
        limit: usize,
        /// Emit the answer and citations as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Start the background indexing daemon (see the `serve` stage).
    Serve,
    /// Start the MCP server (see the `mcp` stage).
    Mcp,
}

/// A retrieval-augmented answer engine, wired in a later stage. Defined as a
/// port so the `ask` handler is testable against a fake without any network.
pub trait AskEngine {
    /// Answer `question` grounded in transcripts matching `scope`, returning the
    /// answer text and the session ids it drew from.
    fn ask(&self, db: &Db, question: &str, scope: &Scope, limit: usize) -> Result<Answer>;
}

/// The result of an [`AskEngine::ask`] call.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Answer {
    /// The generated answer text.
    pub text: String,
    /// Session ids cited as evidence.
    pub citations: Vec<String>,
}

/// Placeholder [`AskEngine`] used until the RAG stage lands: it returns a clear
/// not-yet-available message rather than fabricating an answer.
#[derive(Debug, Default, Clone, Copy)]
pub struct UnavailableAsk;

impl AskEngine for UnavailableAsk {
    fn ask(&self, _db: &Db, _question: &str, _scope: &Scope, _limit: usize) -> Result<Answer> {
        Err(Error::other(
            "ask is not available yet: the retrieval-augmented answer engine is not configured",
        ))
    }
}

/// Handle the `sync` command: run the indexer over `sources` and report stats.
pub fn handle_sync(
    sources: &[Box<dyn SessionSource>],
    db: &Db,
    git: &dyn GitRunner,
    json: bool,
    out: &mut dyn Write,
) -> Result<()> {
    let stats = sync(sources, db, git)?;
    writeln!(
        out,
        "{}",
        render_sync_stats(&stats, Format::from_json_flag(json))
    )?;
    Ok(())
}

/// Handle the `query` command.
pub fn handle_query(
    db: &Db,
    text: &str,
    scope: &Scope,
    limit: usize,
    code: bool,
    json: bool,
    out: &mut dyn Write,
) -> Result<()> {
    let opts = SearchOpts { code, limit };
    let hits = db.search(text, scope, &opts)?;
    writeln!(out, "{}", render_hits(&hits, Format::from_json_flag(json)))?;
    Ok(())
}

/// Handle the `list` command.
pub fn handle_list(db: &Db, json: bool, out: &mut dyn Write) -> Result<()> {
    let sources = db.list_sources()?;
    writeln!(
        out,
        "{}",
        render_sources(&sources, Format::from_json_flag(json))
    )?;
    Ok(())
}

/// Handle the `sessions` command.
pub fn handle_sessions(
    db: &Db,
    scope: &Scope,
    limit: usize,
    json: bool,
    out: &mut dyn Write,
) -> Result<()> {
    let rows = db.list_sessions(scope, limit)?;
    writeln!(
        out,
        "{}",
        render_sessions(&rows, Format::from_json_flag(json))
    )?;
    Ok(())
}

/// Handle the `show` command.
pub fn handle_show(db: &Db, id: &str, json: bool, out: &mut dyn Write) -> Result<()> {
    let msgs = db.session_messages(id)?;
    writeln!(
        out,
        "{}",
        render_messages(id, &msgs, Format::from_json_flag(json))
    )?;
    Ok(())
}

/// Handle the `current` command.
pub fn handle_current(db: &Db, json: bool, out: &mut dyn Write) -> Result<()> {
    let accts = db.current_accounts()?;
    writeln!(
        out,
        "{}",
        render_current(&accts, Format::from_json_flag(json))
    )?;
    Ok(())
}

/// Handle the `ask` command by routing through the [`AskEngine`] port.
pub fn handle_ask(
    db: &Db,
    engine: &dyn AskEngine,
    question: &str,
    scope: &Scope,
    limit: usize,
    json: bool,
    out: &mut dyn Write,
) -> Result<()> {
    let answer = engine.ask(db, question, scope, limit)?;
    writeln!(
        out,
        "{}",
        render_answer(
            &answer.text,
            &answer.citations,
            Format::from_json_flag(json)
        )
    )?;
    Ok(())
}

/// Dispatch a parsed [`Cli`] against injected dependencies, writing all output
/// to `out`. This is the testable core of the CLI; [`run`] supplies the real
/// dependencies.
pub fn dispatch(
    cli: Cli,
    db: &Db,
    sources: &[Box<dyn SessionSource>],
    git: &dyn GitRunner,
    engine: &dyn AskEngine,
    out: &mut dyn Write,
) -> Result<()> {
    match cli.command {
        Some(Command::Sync { json }) => handle_sync(sources, db, git, json, out),
        Some(Command::Query {
            text,
            scope,
            limit,
            code,
            json,
        }) => handle_query(db, &text, &scope.to_scope(), limit, code, json, out),
        Some(Command::List { json }) => handle_list(db, json, out),
        Some(Command::Sessions { scope, limit, json }) => {
            handle_sessions(db, &scope.to_scope(), limit, json, out)
        }
        Some(Command::Show { id, json }) => handle_show(db, &id, json, out),
        Some(Command::Current { json }) => handle_current(db, json, out),
        Some(Command::Ask {
            question,
            scope,
            limit,
            json,
        }) => handle_ask(db, engine, &question, &scope.to_scope(), limit, json, out),
        Some(Command::Serve) => {
            writeln!(out, "serve: the indexing daemon is not available yet")?;
            Ok(())
        }
        Some(Command::Mcp) => {
            writeln!(out, "mcp: the MCP server is not available yet")?;
            Ok(())
        }
        None => {
            writeln!(out, "csx {}", env!("CARGO_PKG_VERSION"))?;
            Ok(())
        }
    }
}

/// Resolve the on-disk database path: `$CSX_DB` if set and non-empty, else
/// `~/.csx/index.sqlite`, else a relative `.csx/index.sqlite` when `HOME` is
/// unset (keeps the resolver total).
pub fn resolve_db_path() -> std::path::PathBuf {
    resolve_db_path_from(std::env::var_os("CSX_DB"), std::env::var_os("HOME"))
}

/// Pure core of [`resolve_db_path`], taking explicit environment values so both
/// the `CSX_DB` override and the `HOME`-relative fallback are deterministically
/// testable without mutating the process environment.
fn resolve_db_path_from(
    csx_db: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> std::path::PathBuf {
    if let Some(p) = csx_db {
        if !p.is_empty() {
            return std::path::PathBuf::from(p);
        }
    }
    home.map(std::path::PathBuf::from)
        .unwrap_or_default()
        .join(".csx")
        .join("index.sqlite")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{MessageRecord, Role, SessionMeta, Tool};
    use crate::source::{ParsedDelta, SessionFile};
    use std::cell::RefCell;
    use std::path::PathBuf;

    // ---- clap parsing -----------------------------------------------------

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(args).expect("parse")
    }

    #[test]
    fn parses_query_with_every_scope_flag() {
        let cli = parse(&[
            "csx",
            "query",
            "the needle",
            "--account",
            "acct-A",
            "--org",
            "org-1",
            "--profile",
            "work",
            "--tool",
            "codex",
            "--repo",
            "repo-x",
            "--cwd",
            "/proj/x",
            "--branch",
            "main",
            "--session",
            "s1",
            "--role",
            "assistant",
            "--tool-call",
            "grep",
            "--since",
            "100",
            "--until",
            "200",
            "--limit",
            "5",
            "--code",
            "--json",
        ]);
        match cli.command.unwrap() {
            Command::Query {
                text,
                scope,
                limit,
                code,
                json,
            } => {
                assert_eq!(text, "the needle");
                assert_eq!(limit, 5);
                assert!(code);
                assert!(json);
                let s = scope.to_scope();
                assert_eq!(s.account_uuid.as_deref(), Some("acct-A"));
                assert_eq!(s.org_uuid.as_deref(), Some("org-1"));
                assert_eq!(s.profile.as_deref(), Some("work"));
                assert_eq!(s.tool.as_deref(), Some("codex"));
                assert_eq!(s.repo_id.as_deref(), Some("repo-x"));
                assert_eq!(s.cwd.as_deref(), Some("/proj/x"));
                assert_eq!(s.branch.as_deref(), Some("main"));
                assert_eq!(s.session_id.as_deref(), Some("s1"));
                assert_eq!(s.role.as_deref(), Some("assistant"));
                assert_eq!(s.tool_name.as_deref(), Some("grep"));
                assert_eq!(s.since, Some(100));
                assert_eq!(s.until, Some(200));
            }
            other => panic!("wrong command: {other:?}"),
        }
    }

    #[test]
    fn query_defaults() {
        let cli = parse(&["csx", "query", "hi"]);
        match cli.command.unwrap() {
            Command::Query {
                limit, code, json, ..
            } => {
                assert_eq!(limit, 20);
                assert!(!code);
                assert!(!json);
            }
            other => panic!("wrong command: {other:?}"),
        }
    }

    #[test]
    fn empty_scope_is_all_none() {
        let s = ScopeArgs::default().to_scope();
        assert!(s.account_uuid.is_none());
        assert!(s.tool.is_none());
        assert!(s.since.is_none());
        assert!(s.tool_name.is_none());
    }

    #[test]
    fn parses_the_other_subcommands() {
        assert!(matches!(
            parse(&["csx", "sync", "--json"]).command.unwrap(),
            Command::Sync { json: true }
        ));
        assert!(matches!(
            parse(&["csx", "list"]).command.unwrap(),
            Command::List { json: false }
        ));
        assert!(matches!(
            parse(&["csx", "sessions", "--tool", "codex", "--limit", "3", "--json"])
                .command
                .unwrap(),
            Command::Sessions {
                limit: 3,
                json: true,
                ..
            }
        ));
        match parse(&["csx", "show", "sess-9"]).command.unwrap() {
            Command::Show { id, json } => {
                assert_eq!(id, "sess-9");
                assert!(!json);
            }
            other => panic!("wrong: {other:?}"),
        }
        assert!(matches!(
            parse(&["csx", "current"]).command.unwrap(),
            Command::Current { json: false }
        ));
        match parse(&["csx", "ask", "why is it slow", "--limit", "4"])
            .command
            .unwrap()
        {
            Command::Ask {
                question, limit, ..
            } => {
                assert_eq!(question, "why is it slow");
                assert_eq!(limit, 4);
            }
            other => panic!("wrong: {other:?}"),
        }
        assert!(matches!(
            parse(&["csx", "serve"]).command.unwrap(),
            Command::Serve
        ));
        assert!(matches!(
            parse(&["csx", "mcp"]).command.unwrap(),
            Command::Mcp
        ));
        assert!(parse(&["csx"]).command.is_none());
    }

    #[test]
    fn unknown_flag_is_a_parse_error() {
        assert!(Cli::try_parse_from(["csx", "query", "x", "--nope"]).is_err());
    }

    // ---- handlers against a fixture db ------------------------------------

    fn meta(id: &str, tool: Tool, repo: &str, branch: &str, first: i64, last: i64) -> SessionMeta {
        SessionMeta {
            session_id: id.into(),
            tool,
            project_path: Some(format!("/proj/{id}")),
            repo_id: Some(repo.into()),
            project_name: Some(format!("proj-{id}")),
            git_branch: Some(branch.into()),
            account: None,
            first_ts: first,
            last_ts: last,
        }
    }

    fn msg(id: &str, tool: Tool, seq: u64, ts: i64, role: Role, text: &str) -> MessageRecord {
        MessageRecord {
            session_id: id.into(),
            tool,
            seq,
            ts,
            role,
            tool_name: None,
            uuid: None,
            text: text.into(),
            cwd: None,
        }
    }

    fn fixture_db() -> Db {
        let db = Db::open(":memory:").unwrap();
        let src = db
            .upsert_source(&crate::db::SourceRow {
                tool: Some("claude-code".into()),
                email: Some("a@example.com".into()),
                org: Some("Acme".into()),
                profile: Some("work".into()),
                account_uuid: Some("acct-A".into()),
                org_uuid: Some("org-1".into()),
                ..Default::default()
            })
            .unwrap();
        db.upsert_session(
            src,
            &meta("s1", Tool::ClaudeCode, "repo-x", "main", 100, 200),
            2,
            None,
        )
        .unwrap();
        db.insert_message(
            src,
            &msg("s1", Tool::ClaudeCode, 0, 100, Role::User, "the parser bug"),
        )
        .unwrap();
        db.insert_message(
            src,
            &msg(
                "s1",
                Tool::ClaudeCode,
                1,
                200,
                Role::Assistant,
                "fix the parser",
            ),
        )
        .unwrap();
        db
    }

    fn no_sources() -> Vec<Box<dyn SessionSource>> {
        Vec::new()
    }

    /// A fake git runner that never gets called in these handler tests.
    struct NoGit;
    impl GitRunner for NoGit {
        fn run(&self, _cwd: &str, _args: &[&str]) -> Result<String> {
            Err(Error::other("git should not be called"))
        }
    }

    fn as_str(buf: &[u8]) -> &str {
        std::str::from_utf8(buf).unwrap()
    }

    #[test]
    fn dispatch_query_human_and_json() {
        let db = fixture_db();
        let git = NoGit;
        let engine = UnavailableAsk;
        let srcs = no_sources();

        let mut buf = Vec::new();
        dispatch(
            parse(&["csx", "query", "parser"]),
            &db,
            &srcs,
            &git,
            &engine,
            &mut buf,
        )
        .unwrap();
        assert!(as_str(&buf).contains("[parser]"));

        let mut buf = Vec::new();
        dispatch(
            parse(&["csx", "query", "parser", "--json"]),
            &db,
            &srcs,
            &git,
            &engine,
            &mut buf,
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(as_str(&buf)).unwrap();
        assert!(!v.as_array().unwrap().is_empty());

        // A scoped query that filters everything out prints the empty banner.
        let mut buf = Vec::new();
        dispatch(
            parse(&["csx", "query", "parser", "--tool", "codex"]),
            &db,
            &srcs,
            &git,
            &engine,
            &mut buf,
        )
        .unwrap();
        assert_eq!(as_str(&buf).trim(), "no matches");
    }

    #[test]
    fn dispatch_query_code_path() {
        let db = fixture_db();
        let mut buf = Vec::new();
        dispatch(
            parse(&["csx", "query", "arse", "--code"]),
            &db,
            &no_sources(),
            &NoGit,
            &UnavailableAsk,
            &mut buf,
        )
        .unwrap();
        // Trigram substring matches inside "parser": a hit is produced (project
        // column is shown), not the empty banner.
        let text = as_str(&buf);
        assert_ne!(text.trim(), "no matches");
        assert!(text.contains("proj-s1"));
    }

    #[test]
    fn dispatch_list_and_current() {
        let db = fixture_db();
        let mut buf = Vec::new();
        dispatch(
            parse(&["csx", "list", "--json"]),
            &db,
            &no_sources(),
            &NoGit,
            &UnavailableAsk,
            &mut buf,
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(as_str(&buf)).unwrap();
        assert_eq!(v[0]["email"], "a@example.com");

        let mut buf = Vec::new();
        dispatch(
            parse(&["csx", "current"]),
            &db,
            &no_sources(),
            &NoGit,
            &UnavailableAsk,
            &mut buf,
        )
        .unwrap();
        assert!(as_str(&buf).contains("claude-code"));
    }

    #[test]
    fn dispatch_sessions_and_show() {
        let db = fixture_db();
        let mut buf = Vec::new();
        dispatch(
            parse(&["csx", "sessions"]),
            &db,
            &no_sources(),
            &NoGit,
            &UnavailableAsk,
            &mut buf,
        )
        .unwrap();
        assert!(as_str(&buf).contains("s1"));

        let mut buf = Vec::new();
        dispatch(
            parse(&["csx", "show", "s1", "--json"]),
            &db,
            &no_sources(),
            &NoGit,
            &UnavailableAsk,
            &mut buf,
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(as_str(&buf)).unwrap();
        assert_eq!(v["session_id"], "s1");
        assert_eq!(v["messages"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn dispatch_none_prints_version() {
        let db = fixture_db();
        let mut buf = Vec::new();
        dispatch(
            parse(&["csx"]),
            &db,
            &no_sources(),
            &NoGit,
            &UnavailableAsk,
            &mut buf,
        )
        .unwrap();
        assert!(as_str(&buf).starts_with("csx "));
    }

    #[test]
    fn dispatch_serve_and_mcp_are_placeholders() {
        let db = fixture_db();
        let mut buf = Vec::new();
        dispatch(
            parse(&["csx", "serve"]),
            &db,
            &no_sources(),
            &NoGit,
            &UnavailableAsk,
            &mut buf,
        )
        .unwrap();
        assert!(as_str(&buf).contains("not available yet"));

        let mut buf = Vec::new();
        dispatch(
            parse(&["csx", "mcp"]),
            &db,
            &no_sources(),
            &NoGit,
            &UnavailableAsk,
            &mut buf,
        )
        .unwrap();
        assert!(as_str(&buf).contains("not available yet"));
    }

    #[test]
    fn dispatch_ask_uses_the_engine() {
        struct FakeAsk;
        impl AskEngine for FakeAsk {
            fn ask(&self, _db: &Db, question: &str, scope: &Scope, limit: usize) -> Result<Answer> {
                assert_eq!(limit, 4);
                assert_eq!(scope.tool.as_deref(), Some("codex"));
                Ok(Answer {
                    text: format!("re: {question}"),
                    citations: vec!["s1".into()],
                })
            }
        }
        let db = fixture_db();
        let mut buf = Vec::new();
        dispatch(
            parse(&[
                "csx", "ask", "why", "--tool", "codex", "--limit", "4", "--json",
            ]),
            &db,
            &no_sources(),
            &NoGit,
            &FakeAsk,
            &mut buf,
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(as_str(&buf)).unwrap();
        assert_eq!(v["answer"], "re: why");
        assert_eq!(v["citations"][0], "s1");
    }

    #[test]
    fn unavailable_ask_errors() {
        let db = fixture_db();
        let err = dispatch(
            parse(&["csx", "ask", "why"]),
            &db,
            &no_sources(),
            &NoGit,
            &UnavailableAsk,
            &mut Vec::new(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("not available yet"));
    }

    // ---- sync handler with a fake source ----------------------------------

    /// A minimal fake source yielding one file with one message, so the `sync`
    /// dispatch path is exercised end to end into the db.
    struct OneShotSource;
    impl SessionSource for OneShotSource {
        fn tool(&self) -> Tool {
            Tool::Codex
        }
        fn discover(&self) -> Result<Vec<SessionFile>> {
            Ok(vec![SessionFile {
                path: PathBuf::from("/logs/a.jsonl"),
                file_key: "k1".into(),
                size: 10,
                mtime: 1,
            }])
        }
        fn parse(&self, _f: &SessionFile, _from: u64) -> Result<ParsedDelta> {
            Ok(ParsedDelta {
                messages: vec![msg("cs1", Tool::Codex, 0, 5, Role::User, "hello")],
                session: meta("cs1", Tool::Codex, "r", "main", 5, 5),
                new_watermark: 10,
            })
        }
    }

    #[test]
    fn dispatch_sync_indexes_and_reports() {
        let db = Db::open(":memory:").unwrap();
        let srcs: Vec<Box<dyn SessionSource>> = vec![Box::new(OneShotSource)];
        let calls = RefCell::new(0usize);
        struct CountingGit<'a>(&'a RefCell<usize>);
        impl GitRunner for CountingGit<'_> {
            fn run(&self, _cwd: &str, _args: &[&str]) -> Result<String> {
                *self.0.borrow_mut() += 1;
                // Return an empty remote so repo id falls back to cwd.
                Ok(String::new())
            }
        }
        let git = CountingGit(&calls);

        let mut buf = Vec::new();
        dispatch(
            parse(&["csx", "sync", "--json"]),
            &db,
            &srcs,
            &git,
            &UnavailableAsk,
            &mut buf,
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(as_str(&buf)).unwrap();
        assert_eq!(v["messages_added"], 1);
        assert_eq!(v["sessions_touched"], 1);

        // Human variant too.
        let mut buf = Vec::new();
        dispatch(
            parse(&["csx", "sync"]),
            &db,
            &srcs,
            &git,
            &UnavailableAsk,
            &mut buf,
        )
        .unwrap();
        assert!(as_str(&buf).contains("sync:"));
    }

    // ---- pure helpers -----------------------------------------------------

    #[test]
    fn resolve_db_path_from_covers_both_branches() {
        use std::ffi::OsString;
        // An explicit, non-empty CSX_DB wins outright.
        assert_eq!(
            resolve_db_path_from(Some(OsString::from("/custom/db.sqlite")), None),
            std::path::PathBuf::from("/custom/db.sqlite")
        );
        // An empty CSX_DB is ignored, falling back to HOME-relative.
        assert_eq!(
            resolve_db_path_from(Some(OsString::from("")), Some(OsString::from("/home/x"))),
            std::path::PathBuf::from("/home/x/.csx/index.sqlite")
        );
        // No CSX_DB, no HOME: relative default.
        assert_eq!(
            resolve_db_path_from(None, None),
            std::path::PathBuf::from(".csx/index.sqlite")
        );
        // The env-reading wrapper resolves to something ending in the db file.
        assert!(resolve_db_path().ends_with("index.sqlite") || resolve_db_path().is_absolute());
    }

    /// A writer that fails on every write, to exercise the `?` error-propagation
    /// arms of each handler's `writeln!`.
    struct FailWriter;
    impl Write for FailWriter {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("boom"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn handlers_propagate_write_errors() {
        let db = fixture_db();
        let scope = Scope::default();

        handle_query(&db, "parser", &scope, 5, false, false, &mut FailWriter).unwrap_err();
        handle_list(&db, false, &mut FailWriter).unwrap_err();
        handle_sessions(&db, &scope, 5, false, &mut FailWriter).unwrap_err();
        handle_show(&db, "s1", false, &mut FailWriter).unwrap_err();
        handle_current(&db, false, &mut FailWriter).unwrap_err();

        // A sync whose stats render but fail to write.
        let srcs: Vec<Box<dyn SessionSource>> = vec![Box::new(OneShotSource)];
        struct OkGit;
        impl GitRunner for OkGit {
            fn run(&self, _cwd: &str, _args: &[&str]) -> Result<String> {
                Ok(String::new())
            }
        }
        handle_sync(&srcs, &db, &OkGit, false, &mut FailWriter).unwrap_err();

        // The ask handler writes the engine's answer; use a stub that succeeds.
        struct OkAsk;
        impl AskEngine for OkAsk {
            fn ask(&self, _db: &Db, _q: &str, _s: &Scope, _l: usize) -> Result<Answer> {
                Ok(Answer {
                    text: "hi".into(),
                    citations: vec!["s1".into()],
                })
            }
        }
        handle_ask(&db, &OkAsk, "why", &scope, 3, false, &mut FailWriter).unwrap_err();
    }

    #[test]
    fn nogit_runner_reports_unexpected_calls() {
        // The `NoGit` fake guards handler tests that must not touch git; assert
        // it surfaces an error if ever invoked.
        let err = NoGit.run("/tmp", &["rev-parse"]).unwrap_err();
        assert!(err.to_string().contains("git should not be called"));
    }
}
