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
use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::alfred::InstallReport;

use crate::db::{Db, Scope, SearchOpts};
use crate::error::{Error, Result};
use crate::git_shim::GitRunner;
use crate::index::sync;
use crate::output::{
    render_answer, render_current, render_hits, render_messages, render_sessions, render_sources,
    render_sync_stats, Format,
};
use crate::source::SessionSource;
use crate::tabs::app::ListOptions;
use crate::tabs::render::{render_alfred_with_home, render_human, render_json};
use crate::tabs::TabsReport;

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

/// Subcommands of `csx tabs`.
#[derive(Debug, Subcommand)]
pub enum TabsAction {
    /// Focus a tab by ref.
    Focus {
        /// A ref from `csx tabs`, such as `ghostty:<id>` or `tty:ttys012`.
        tab_ref: String,
        /// kitty remote-control socket, such as `unix:/tmp/kitty`.
        #[arg(long)]
        kitty_socket: Option<String>,
    },
}

/// Subcommands of `csx alfred`.
#[derive(Debug, Subcommand)]
pub enum AlfredAction {
    /// Install or update the workflow.
    Install {
        /// Alfred preferences folder; found automatically when omitted.
        #[arg(long)]
        prefs: Option<PathBuf>,
    },
}

/// How `csx tabs` prints its report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabsOutput {
    /// Aligned text for people.
    Human,
    /// `{"rows": [...], "warnings": [...]}`.
    Json,
    /// Alfred script filter JSON.
    Alfred,
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
    /// List terminal tabs and the AI agents running in them, or focus one.
    #[command(args_conflicts_with_subcommands = true)]
    Tabs {
        /// Focus a tab instead of listing.
        #[command(subcommand)]
        action: Option<TabsAction>,
        /// Show only tabs that run an agent.
        #[arg(long)]
        agents: bool,
        /// Show only this terminal (repeatable): ghostty, iterm2, terminal,
        /// kitty, wezterm, tmux.
        #[arg(long = "terminal")]
        terminals: Vec<String>,
        /// Emit `{"rows": [...], "warnings": [...]}` as JSON.
        #[arg(long)]
        json: bool,
        /// Emit Alfred script filter JSON.
        #[arg(long, conflicts_with = "json")]
        alfred: bool,
        /// kitty remote-control socket, such as `unix:/tmp/kitty-1234`. Default:
        /// `KITTY_LISTEN_ON`, else the `/tmp/kitty-<pid>` of each running kitty.
        #[arg(long)]
        kitty_socket: Option<String>,
        /// Words that every listed row must match.
        query: Vec<String>,
    },
    /// Manage the Alfred workflow.
    Alfred {
        /// What to do.
        #[command(subcommand)]
        action: AlfredAction,
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
    /// Start the background indexing daemon: watch the source roots, debounce
    /// re-indexing, and answer scoped queries over a Unix socket.
    Serve,
    /// Start the MCP server: speak JSON-RPC over stdio so an agent can search,
    /// fetch, and ask over the indexed sessions as tools.
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

/// Port that runs the background indexing/serving daemon.
///
/// Defined as a port so the `serve` dispatch arm is exercised by a fake in
/// tests; the real adapter ([`crate::cli_shim`]) constructs the filesystem
/// watcher, Unix-socket listener, and system clock and runs the daemon loops.
pub trait DaemonRunner {
    /// Run the daemon over `sources`/`db` until a watcher or listener error
    /// stops it.
    fn serve(&self, db: &Db, sources: &[Box<dyn SessionSource>], git: &dyn GitRunner)
        -> Result<()>;
}

/// Port that runs the MCP stdio server.
///
/// Defined as a port so the `mcp` dispatch arm is exercised by a fake in tests;
/// the real adapter ([`crate::cli_shim`]) pumps the [`crate::mcp`] handler over
/// the process's stdin/stdout.
pub trait McpRunner {
    /// Serve MCP requests against `db` (and `engine`) until stdin reaches EOF.
    fn serve(&self, db: &Db, engine: &dyn AskEngine) -> Result<()>;
}

/// Port for the terminal tab features.
///
/// Defined as a port so `dispatch` reaches the tabs code only through it; the
/// real adapter ([`crate::cli_shim`]) builds the OS ports and the agent
/// context, and answers "macOS only" on other platforms.
pub trait TabsRunner {
    /// List tabs and agents.
    fn list(&self, db: &Db, opts: &ListOptions, kitty_socket: Option<String>)
        -> Result<TabsReport>;
    /// Focus the tab named by `tab_ref`.
    fn focus(&self, tab_ref: &str, kitty_socket: Option<String>) -> Result<()>;
    /// Install or update the Alfred workflow; `prefs` overrides discovery.
    fn alfred_install(&self, prefs: Option<PathBuf>) -> Result<InstallReport>;
    /// The user's home directory, when known; used to shorten paths.
    fn home(&self) -> Option<PathBuf>;
}

/// The kitty socket to use: the flag, else `KITTY_LISTEN_ON`; empty values
/// count as unset. When both are unset, csx probes `/tmp/kitty-<pid>`.
pub fn resolve_kitty_socket(flag: Option<String>, env: Option<String>) -> Option<String> {
    flag.filter(|s| !s.is_empty())
        .or_else(|| env.filter(|s| !s.is_empty()))
}

/// What `AgentContext::home` holds when the home directory is unknown. It does
/// not exist, so nothing is read through a path built from it.
const NO_HOME: &str = "/nonexistent";

/// Build the agent context from explicit environment values. Empty values count
/// as unset. Claude dirs are `~/.claude`, then the dirs the index knows, then
/// `CLAUDE_CONFIG_DIR`, without duplicates and in that order. The Codex home is
/// `CODEX_HOME`, else `~/.codex`. With no home, nothing is derived from it:
/// `~/.claude` is left out and the Codex home falls back to [`NO_HOME`].
pub fn tabs_context(
    home: Option<PathBuf>,
    indexed_claude_dirs: Vec<String>,
    claude_config_dir: Option<PathBuf>,
    codex_home: Option<PathBuf>,
    now_ms: i64,
) -> crate::agent::AgentContext {
    let home = home.filter(|h| !h.as_os_str().is_empty());
    let mut candidates: Vec<PathBuf> = home.iter().map(|h| h.join(".claude")).collect();
    candidates.extend(indexed_claude_dirs.into_iter().map(PathBuf::from));
    candidates.extend(claude_config_dir);
    let mut claude_dirs: Vec<PathBuf> = Vec::new();
    for dir in candidates {
        if !dir.as_os_str().is_empty() && !claude_dirs.contains(&dir) {
            claude_dirs.push(dir);
        }
    }
    let codex_home = codex_home
        .filter(|c| !c.as_os_str().is_empty())
        .or_else(|| home.as_ref().map(|h| h.join(".codex")))
        .unwrap_or_else(|| PathBuf::from(NO_HOME));
    crate::agent::AgentContext {
        home: home.unwrap_or_else(|| PathBuf::from(NO_HOME)),
        claude_dirs,
        codex_home,
        now_ms,
    }
}

/// Handle `csx tabs`: list through the runner and render in `output`.
/// `home` shortens working directories in the Alfred output.
pub fn handle_tabs(
    db: &Db,
    tabs: &dyn TabsRunner,
    opts: &ListOptions,
    kitty_socket: Option<String>,
    output: TabsOutput,
    home: Option<&std::path::Path>,
    out: &mut dyn Write,
) -> Result<()> {
    let report = tabs.list(db, opts, kitty_socket)?;
    let text = match output {
        TabsOutput::Human => render_human(&report),
        TabsOutput::Json => render_json(&report),
        TabsOutput::Alfred => {
            render_alfred_with_home(&report, home.and_then(std::path::Path::to_str))
        }
    };
    writeln!(out, "{}", text.trim_end_matches('\n'))?;
    Ok(())
}

/// Handle `csx alfred install`: install and report what happened.
pub fn handle_alfred_install(
    tabs: &dyn TabsRunner,
    prefs: Option<PathBuf>,
    out: &mut dyn Write,
) -> Result<()> {
    let report = tabs.alfred_install(prefs)?;
    writeln!(out, "installed the workflow in {}", report.dir.display())?;
    if let Some(keyword) = &report.migrated_keyword {
        writeln!(out, "migrated keyword {keyword}")?;
    }
    if report.removed_old {
        writeln!(out, "removed the old Claude Tabs workflow")?;
    }
    writeln!(
        out,
        "macOS asks once to allow Alfred to control each terminal app"
    )?;
    Ok(())
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
// A wiring hub: it forwards each command to a handler using one injected port
// per side effect (db, sources, git, ask, daemon, mcp, tabs) plus the output sink.
// Bundling these into a struct would only relocate the same fan-out.
#[allow(clippy::too_many_arguments)]
pub fn dispatch(
    cli: Cli,
    db: &Db,
    sources: &[Box<dyn SessionSource>],
    git: &dyn GitRunner,
    engine: &dyn AskEngine,
    daemon: &dyn DaemonRunner,
    mcp: &dyn McpRunner,
    tabs: &dyn TabsRunner,
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
        Some(Command::Serve) => daemon.serve(db, sources, git),
        Some(Command::Mcp) => mcp.serve(db, engine),
        Some(Command::Tabs {
            action:
                Some(TabsAction::Focus {
                    tab_ref,
                    kitty_socket,
                }),
            ..
        }) => tabs.focus(&tab_ref, kitty_socket),
        Some(Command::Tabs {
            action: None,
            agents,
            terminals,
            json,
            alfred,
            kitty_socket,
            query,
        }) => {
            let output = match (json, alfred) {
                (true, _) => TabsOutput::Json,
                (_, true) => TabsOutput::Alfred,
                _ => TabsOutput::Human,
            };
            let opts = ListOptions {
                agents_only: agents,
                terminals,
                query,
            };
            let home = tabs.home();
            handle_tabs(db, tabs, &opts, kitty_socket, output, home.as_deref(), out)
        }
        Some(Command::Alfred {
            action: AlfredAction::Install { prefs },
        }) => handle_alfred_install(tabs, prefs, out),
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

/// Resolve the daemon's Unix-socket path: `$CSX_SOCK` if set and non-empty, else
/// `~/.csx/csx.sock`, else a relative `.csx/csx.sock` when `HOME` is unset.
pub fn resolve_sock_path() -> std::path::PathBuf {
    resolve_sock_path_from(std::env::var_os("CSX_SOCK"), std::env::var_os("HOME"))
}

/// Pure core of [`resolve_sock_path`], taking explicit environment values so the
/// override and the `HOME`-relative fallback are deterministically testable.
fn resolve_sock_path_from(
    csx_sock: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> std::path::PathBuf {
    if let Some(p) = csx_sock {
        if !p.is_empty() {
            return std::path::PathBuf::from(p);
        }
    }
    home.map(std::path::PathBuf::from)
        .unwrap_or_default()
        .join(".csx")
        .join("csx.sock")
}

/// The set of filesystem roots the daemon should watch: each configured
/// source's config directory, when it exposes one. Deduplication and existence
/// are left to the watcher adapter (a missing root is watched benignly).
pub fn watch_roots(sources: &[Box<dyn SessionSource>]) -> Vec<std::path::PathBuf> {
    sources
        .iter()
        .filter_map(|s| s.config_dir().map(std::path::PathBuf::from))
        .collect()
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

    /// A daemon/MCP runner fake: both ports resolve to a recorded, side-effect
    /// free `Ok(())`, so the `serve`/`mcp` dispatch arms are exercised without
    /// binding sockets, watching the filesystem, or touching stdio.
    #[derive(Default)]
    struct FakeRunner {
        served: std::cell::Cell<bool>,
        mcp: std::cell::Cell<bool>,
    }
    impl DaemonRunner for FakeRunner {
        fn serve(
            &self,
            _db: &Db,
            _sources: &[Box<dyn SessionSource>],
            _git: &dyn GitRunner,
        ) -> Result<()> {
            self.served.set(true);
            Ok(())
        }
    }
    impl McpRunner for FakeRunner {
        fn serve(&self, _db: &Db, _engine: &dyn AskEngine) -> Result<()> {
            self.mcp.set(true);
            Ok(())
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
            &FakeRunner::default(),
            &FakeRunner::default(),
            &NoTabs,
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
            &FakeRunner::default(),
            &FakeRunner::default(),
            &NoTabs,
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
            &FakeRunner::default(),
            &FakeRunner::default(),
            &NoTabs,
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
            &FakeRunner::default(),
            &FakeRunner::default(),
            &NoTabs,
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
            &FakeRunner::default(),
            &FakeRunner::default(),
            &NoTabs,
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
            &FakeRunner::default(),
            &FakeRunner::default(),
            &NoTabs,
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
            &FakeRunner::default(),
            &FakeRunner::default(),
            &NoTabs,
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
            &FakeRunner::default(),
            &FakeRunner::default(),
            &NoTabs,
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
            &FakeRunner::default(),
            &FakeRunner::default(),
            &NoTabs,
            &mut buf,
        )
        .unwrap();
        assert!(as_str(&buf).starts_with("csx "));
    }

    #[test]
    fn dispatch_serve_and_mcp_route_to_their_runners() {
        let db = fixture_db();
        let runner = FakeRunner::default();

        let mut buf = Vec::new();
        dispatch(
            parse(&["csx", "serve"]),
            &db,
            &no_sources(),
            &NoGit,
            &UnavailableAsk,
            &runner,
            &runner,
            &NoTabs,
            &mut buf,
        )
        .unwrap();
        assert!(runner.served.get(), "serve routed to the daemon runner");
        assert!(!runner.mcp.get());
        assert!(as_str(&buf).is_empty());

        let mut buf = Vec::new();
        dispatch(
            parse(&["csx", "mcp"]),
            &db,
            &no_sources(),
            &NoGit,
            &UnavailableAsk,
            &runner,
            &runner,
            &NoTabs,
            &mut buf,
        )
        .unwrap();
        assert!(runner.mcp.get(), "mcp routed to the MCP runner");
        assert!(as_str(&buf).is_empty());
    }

    /// A runner whose ports both fail, proving dispatch propagates a runner
    /// error rather than swallowing it.
    #[test]
    fn dispatch_serve_and_mcp_propagate_runner_errors() {
        struct FailRunner;
        impl DaemonRunner for FailRunner {
            fn serve(
                &self,
                _db: &Db,
                _sources: &[Box<dyn SessionSource>],
                _git: &dyn GitRunner,
            ) -> Result<()> {
                Err(Error::other("daemon boom"))
            }
        }
        impl McpRunner for FailRunner {
            fn serve(&self, _db: &Db, _engine: &dyn AskEngine) -> Result<()> {
                Err(Error::other("mcp boom"))
            }
        }
        let db = fixture_db();
        let err = dispatch(
            parse(&["csx", "serve"]),
            &db,
            &no_sources(),
            &NoGit,
            &UnavailableAsk,
            &FailRunner,
            &FailRunner,
            &NoTabs,
            &mut Vec::new(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("daemon boom"));

        let err = dispatch(
            parse(&["csx", "mcp"]),
            &db,
            &no_sources(),
            &NoGit,
            &UnavailableAsk,
            &FailRunner,
            &FailRunner,
            &NoTabs,
            &mut Vec::new(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("mcp boom"));
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
            &FakeRunner::default(),
            &FakeRunner::default(),
            &NoTabs,
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
            &FakeRunner::default(),
            &FakeRunner::default(),
            &NoTabs,
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
            &FakeRunner::default(),
            &FakeRunner::default(),
            &NoTabs,
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
            &FakeRunner::default(),
            &FakeRunner::default(),
            &NoTabs,
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

    #[test]
    fn resolve_sock_path_from_covers_both_branches() {
        use std::ffi::OsString;
        // An explicit, non-empty CSX_SOCK wins outright.
        assert_eq!(
            resolve_sock_path_from(Some(OsString::from("/run/csx.sock")), None),
            std::path::PathBuf::from("/run/csx.sock")
        );
        // An empty CSX_SOCK is ignored, falling back to HOME-relative.
        assert_eq!(
            resolve_sock_path_from(Some(OsString::from("")), Some(OsString::from("/home/x"))),
            std::path::PathBuf::from("/home/x/.csx/csx.sock")
        );
        // No CSX_SOCK, no HOME: relative default.
        assert_eq!(
            resolve_sock_path_from(None, None),
            std::path::PathBuf::from(".csx/csx.sock")
        );
        // The env-reading wrapper resolves to the socket file or an absolute path.
        assert!(resolve_sock_path().ends_with("csx.sock") || resolve_sock_path().is_absolute());
    }

    #[test]
    fn watch_roots_collects_source_config_dirs() {
        use crate::source::{ClaudeSource, CodexSource};
        // Two real sources that expose a config dir, plus `OneShotSource` which
        // uses the trait default (`None`), so the `filter_map` drops it. Reusing
        // fakes whose other methods are already covered elsewhere keeps this
        // test from introducing new, unexercised trait impls.
        let sources: Vec<Box<dyn SessionSource>> = vec![
            Box::new(ClaudeSource::new("/a/.claude")),
            Box::new(OneShotSource),
            Box::new(CodexSource::new("/b/.codex")),
        ];
        assert_eq!(
            watch_roots(&sources),
            vec![
                std::path::PathBuf::from("/a/.claude"),
                std::path::PathBuf::from("/b/.codex"),
            ]
        );
        // No sources → nothing to watch.
        assert!(watch_roots(&no_sources()).is_empty());
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

    // ---- tabs and alfred ---------------------------------------------------

    /// A writer that accepts `ok_lines` newline-terminated writes, then fails.
    struct LimitWriter {
        ok_lines: usize,
    }
    impl Write for LimitWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if buf.contains(&b'\n') {
                if self.ok_lines == 0 {
                    return Err(std::io::Error::other("boom"));
                }
                self.ok_lines -= 1;
            }
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn tabs_and_alfred_handlers_propagate_write_errors() {
        let db = fixture_db();
        let tabs = FakeTabs {
            install: Some(install_report(Some("ct"), true)),
            ..Default::default()
        };
        let opts = ListOptions::default();
        handle_tabs(
            &db,
            &tabs,
            &opts,
            None,
            TabsOutput::Human,
            None,
            &mut FailWriter,
        )
        .unwrap_err();
        // The report has four lines, so failing after 0..=3 lines hits each write.
        for ok_lines in 0..4 {
            handle_alfred_install(&tabs, None, &mut LimitWriter { ok_lines }).unwrap_err();
        }
        handle_alfred_install(&tabs, None, &mut LimitWriter { ok_lines: 4 }).unwrap();
    }

    /// Tabs runner for tests that never reach the tabs code.
    struct NoTabs;
    impl TabsRunner for NoTabs {
        fn list(&self, _: &Db, _: &ListOptions, _: Option<String>) -> Result<TabsReport> {
            Err(Error::other("tabs should not be called"))
        }
        fn focus(&self, _: &str, _: Option<String>) -> Result<()> {
            Err(Error::other("tabs should not be called"))
        }
        fn alfred_install(&self, _: Option<PathBuf>) -> Result<InstallReport> {
            Err(Error::other("tabs should not be called"))
        }
        fn home(&self) -> Option<PathBuf> {
            None
        }
    }

    /// Records what dispatch passes to the tabs runner and answers from fields.
    #[derive(Default)]
    struct FakeTabs {
        report: TabsReport,
        install: Option<InstallReport>,
        fail: bool,
        home: Option<PathBuf>,
        seen: RefCell<Vec<String>>,
    }
    impl TabsRunner for FakeTabs {
        fn list(
            &self,
            _: &Db,
            opts: &ListOptions,
            kitty_socket: Option<String>,
        ) -> Result<TabsReport> {
            self.seen
                .borrow_mut()
                .push(format!("list {opts:?} {kitty_socket:?}"));
            if self.fail {
                return Err(Error::other("tabs boom"));
            }
            Ok(self.report.clone())
        }
        fn focus(&self, tab_ref: &str, kitty_socket: Option<String>) -> Result<()> {
            self.seen
                .borrow_mut()
                .push(format!("focus {tab_ref} {kitty_socket:?}"));
            if self.fail {
                return Err(Error::other("tabs boom"));
            }
            Ok(())
        }
        fn alfred_install(&self, prefs: Option<PathBuf>) -> Result<InstallReport> {
            self.seen.borrow_mut().push(format!("install {prefs:?}"));
            if self.fail {
                return Err(Error::other("tabs boom"));
            }
            Ok(self.install.clone().unwrap())
        }
        fn home(&self) -> Option<PathBuf> {
            self.home.clone()
        }
    }

    fn run_tabs(args: &[&str], tabs: &FakeTabs) -> Result<String> {
        let mut buf = Vec::new();
        dispatch(
            parse(args),
            &fixture_db(),
            &no_sources(),
            &NoGit,
            &UnavailableAsk,
            &FakeRunner::default(),
            &FakeRunner::default(),
            tabs,
            &mut buf,
        )?;
        Ok(as_str(&buf).to_string())
    }

    fn one_tab_report() -> TabsReport {
        TabsReport {
            rows: vec![crate::tabs::Tab {
                terminal: "ghostty".into(),
                ref_: Some("ghostty:abc".into()),
                title: "my tab".into(),
                window: 1,
                index: 1,
                ..Default::default()
            }],
            warnings: vec!["kitty: no socket".into()],
        }
    }

    #[test]
    fn parses_tabs_flags_and_query() {
        let cli = parse(&[
            "csx",
            "tabs",
            "--agents",
            "--terminal",
            "ghostty",
            "--terminal",
            "tmux",
            "--json",
            "--kitty-socket",
            "unix:/tmp/k",
            "fix",
            "bug",
        ]);
        assert_eq!(
            format!("{:?}", cli.command),
            "Some(Tabs { action: None, agents: true, terminals: [\"ghostty\", \"tmux\"], \
             json: true, alfred: false, kitty_socket: Some(\"unix:/tmp/k\"), \
             query: [\"fix\", \"bug\"] })"
        );
    }

    #[test]
    fn parses_tabs_focus_subcommand() {
        assert_eq!(
            format!(
                "{:?}",
                parse(&["csx", "tabs", "focus", "tty:ttys012"]).command
            ),
            "Some(Tabs { action: Some(Focus { tab_ref: \"tty:ttys012\", kitty_socket: None }), agents: false, \
             terminals: [], json: false, alfred: false, kitty_socket: None, query: [] })"
        );
        assert!(format!(
            "{:?}",
            parse(&[
                "csx",
                "tabs",
                "focus",
                "kitty:3",
                "--kitty-socket",
                "unix:/k"
            ])
            .command
        )
        .contains("kitty_socket: Some(\"unix:/k\") }"));
        // After a list flag, "focus" is an ordinary query word.
        let cli = parse(&["csx", "tabs", "--agents", "focus", "x"]);
        assert!(format!("{:?}", cli.command).contains("action: None"));
        assert!(format!("{:?}", cli.command).contains("query: [\"focus\", \"x\"]"));
    }

    #[test]
    fn alfred_conflicts_with_json() {
        assert!(Cli::try_parse_from(["csx", "tabs", "--alfred", "--json"]).is_err());
        assert!(Cli::try_parse_from(["csx", "tabs", "--alfred"]).is_ok());
    }

    #[test]
    fn parses_alfred_install_prefs() {
        let cli = parse(&[
            "csx",
            "alfred",
            "install",
            "--prefs",
            "/p/Alfred.alfredpreferences",
        ]);
        assert_eq!(
            format!("{:?}", cli.command),
            "Some(Alfred { action: Install { prefs: Some(\"/p/Alfred.alfredpreferences\") } })"
        );
        assert_eq!(
            format!("{:?}", parse(&["csx", "alfred", "install"]).command),
            "Some(Alfred { action: Install { prefs: None } })"
        );
    }

    #[test]
    fn dispatch_tabs_renders_each_output() {
        let tabs = FakeTabs {
            report: one_tab_report(),
            ..Default::default()
        };
        let human = run_tabs(&["csx", "tabs"], &tabs).unwrap();
        assert!(human.contains("TERMINAL") && human.contains("my tab"));
        assert!(human.contains("warning: kitty: no socket"));
        assert!(human.ends_with("ghostty:abc\n") && !human.ends_with("\n\n"));

        let json = run_tabs(&["csx", "tabs", "--json"], &tabs).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["rows"][0]["ref"], "ghostty:abc");
        assert_eq!(v["warnings"][0], "kitty: no socket");

        let alfred = run_tabs(&["csx", "tabs", "--alfred"], &tabs).unwrap();
        let v: serde_json::Value = serde_json::from_str(&alfred).unwrap();
        assert!(v["items"].as_array().unwrap().len() >= 2);

        // Options reach the runner.
        run_tabs(
            &[
                "csx",
                "tabs",
                "--agents",
                "--terminal",
                "kitty",
                "--kitty-socket",
                "s",
                "a",
                "b",
            ],
            &tabs,
        )
        .unwrap();
        let seen = tabs.seen.borrow();
        let last = seen.last().unwrap();
        assert!(last.contains("agents_only: true"), "{last}");
        assert!(
            last.contains("\"kitty\"") && last.contains("\"a\", \"b\""),
            "{last}"
        );
        assert!(last.ends_with("Some(\"s\")"), "{last}");
    }

    #[test]
    fn dispatch_tabs_focus_calls_runner() {
        let tabs = FakeTabs::default();
        let out = run_tabs(&["csx", "tabs", "focus", "ghostty:abc"], &tabs).unwrap();
        assert!(out.is_empty());
        run_tabs(
            &[
                "csx",
                "tabs",
                "focus",
                "kitty:2",
                "--kitty-socket",
                "unix:/k",
            ],
            &tabs,
        )
        .unwrap();
        assert_eq!(tabs.seen.borrow()[1], "focus kitty:2 Some(\"unix:/k\")");
        tabs.seen.borrow_mut().truncate(1);
        assert_eq!(*tabs.seen.borrow(), ["focus ghostty:abc None"]);
    }

    fn install_report(migrated: Option<&str>, removed_old: bool) -> InstallReport {
        InstallReport {
            dir: PathBuf::from("/prefs/workflows/csx-tabs"),
            migrated_keyword: migrated.map(str::to_string),
            removed_old,
        }
    }

    #[test]
    fn dispatch_alfred_install_prints_report() {
        let notice = "macOS asks once to allow Alfred to control each terminal app";
        let tabs = FakeTabs {
            install: Some(install_report(Some("ct"), true)),
            ..Default::default()
        };
        let out = run_tabs(&["csx", "alfred", "install", "--prefs", "/p"], &tabs).unwrap();
        assert!(out.contains("/prefs/workflows/csx-tabs"));
        assert!(out.contains("migrated keyword ct"));
        assert!(out.contains("removed the old Claude Tabs workflow"));
        assert!(out.contains(notice));
        assert_eq!(*tabs.seen.borrow(), ["install Some(\"/p\")"]);

        let tabs = FakeTabs {
            install: Some(install_report(None, false)),
            ..Default::default()
        };
        let out = run_tabs(&["csx", "alfred", "install"], &tabs).unwrap();
        assert!(!out.contains("migrated") && !out.contains("removed"));
        assert!(out.contains(notice));
        assert_eq!(*tabs.seen.borrow(), ["install None"]);
    }

    #[test]
    fn dispatch_tabs_propagates_runner_error() {
        let tabs = FakeTabs {
            fail: true,
            ..Default::default()
        };
        for args in [
            &["csx", "tabs"][..],
            &["csx", "tabs", "focus", "x:y"],
            &["csx", "alfred", "install"],
        ] {
            let err = run_tabs(args, &tabs).unwrap_err();
            assert!(err.to_string().contains("tabs boom"), "{args:?}");
        }
    }

    #[test]
    fn no_tabs_fake_errors_on_every_port() {
        let db = fixture_db();
        assert!(NoTabs.list(&db, &ListOptions::default(), None).is_err());
        assert!(NoTabs.focus("x", None).is_err());
        assert!(NoTabs.alfred_install(None).is_err());
        assert_eq!(NoTabs.home(), None);
    }

    #[test]
    fn alfred_output_shortens_the_runner_home() {
        let mut report = one_tab_report();
        report.rows[0].cwd = Some("/Users/me/proj".into());
        let tabs = FakeTabs {
            report,
            home: Some(PathBuf::from("/Users/me")),
            ..Default::default()
        };
        let out = run_tabs(&["csx", "tabs", "--alfred"], &tabs).unwrap();
        assert!(out.contains("~/proj"), "{out}");
        let tabs = FakeTabs {
            report: tabs.report.clone(),
            ..Default::default()
        };
        let out = run_tabs(&["csx", "tabs", "--alfred"], &tabs).unwrap();
        assert!(
            !out.contains("~/proj") && out.contains("/Users/me/proj"),
            "{out}"
        );
    }

    #[test]
    fn kitty_socket_flag_beats_env_and_empty_is_unset() {
        let s = |v: &str| Some(v.to_string());
        assert_eq!(resolve_kitty_socket(s("f"), s("e")), s("f"));
        assert_eq!(resolve_kitty_socket(None, s("e")), s("e"));
        assert_eq!(resolve_kitty_socket(s(""), s("e")), s("e"));
        assert_eq!(resolve_kitty_socket(s(""), s("")), None);
        assert_eq!(resolve_kitty_socket(None, None), None);
    }

    #[test]
    fn tabs_context_orders_dirs_and_defaults_codex_home() {
        let home = Some(PathBuf::from("/h"));
        let ctx = tabs_context(
            home.clone(),
            vec!["/idx/.claude".into(), "/h/.claude".into(), "".into()],
            Some(PathBuf::from("/env/claude")),
            None,
            42,
        );
        // Duplicates and empty values are dropped; order is kept.
        assert_eq!(
            ctx.claude_dirs,
            [
                PathBuf::from("/h/.claude"),
                PathBuf::from("/idx/.claude"),
                PathBuf::from("/env/claude")
            ]
        );
        assert_eq!(ctx.codex_home, PathBuf::from("/h/.codex"));
        assert_eq!((ctx.home, ctx.now_ms), (PathBuf::from("/h"), 42));

        let ctx = tabs_context(home.clone(), vec![], None, Some("/c".into()), 0);
        assert_eq!(ctx.claude_dirs, [PathBuf::from("/h/.claude")]);
        assert_eq!(ctx.codex_home, PathBuf::from("/c"));

        // Empty env values are ignored.
        let ctx = tabs_context(home, vec![], Some("".into()), Some("".into()), 0);
        assert_eq!(ctx.claude_dirs, [PathBuf::from("/h/.claude")]);
        assert_eq!(ctx.codex_home, PathBuf::from("/h/.codex"));
    }

    #[test]
    fn tabs_context_without_home_builds_no_relative_paths() {
        for home in [None, Some(PathBuf::new())] {
            let ctx = tabs_context(home, vec![], Some("/env/claude".into()), None, 1);
            assert_eq!(ctx.claude_dirs, [PathBuf::from("/env/claude")]);
            assert_eq!(ctx.codex_home, PathBuf::from("/nonexistent"));
            assert_eq!(ctx.home, PathBuf::from("/nonexistent"));
            assert!(ctx.codex_home.is_absolute() && ctx.home.is_absolute());
        }
        let ctx = tabs_context(None, vec![], None, Some("/c".into()), 1);
        assert!(ctx.claude_dirs.is_empty());
        assert_eq!(ctx.codex_home, PathBuf::from("/c"));
    }
}
