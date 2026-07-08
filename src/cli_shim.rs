//! Impure CLI entry point (EXCLUDED from coverage).
//!
//! This is the only place that resolves the real process environment: it parses
//! `std::env::args`, discovers the concrete filesystem sources and the
//! network-backed `ask` engine from environment variables, creates the parent
//! directory of and opens the on-disk database, and writes to the real locked
//! stdout. Every branch-worthy decision it needs is delegated to tested pure
//! functions in [`crate::cli`] ([`resolve_db_path`] and
//! [`dispatch`](crate::cli::dispatch)); this file only wires concrete adapters
//! onto real OS handles, so it holds no logic a test could meaningfully reach.

use clap::Parser;

use crate::cli::{
    dispatch, resolve_db_path, resolve_sock_path, watch_roots, AskEngine, Cli, DaemonRunner,
    McpRunner, UnavailableAsk,
};
use crate::daemon::{serve_loop, watch_loop, Debouncer};
use crate::db::Db;
use crate::error::{Error, Result};
use crate::git_shim::{GitRunner, ProcessGit};
use crate::source::{ClaudeSource, CodexSource, SessionSource};
use crate::transport_shim::UnixSocketListener;
use crate::watch_shim::NotifyWatcher;
use crate::SystemClock;

/// Quiet-window, in milliseconds, the daemon waits after the last observed
/// change before it re-indexes; long enough to coalesce an editor's write burst.
const DEBOUNCE_MS: u64 = 500;

/// Real [`DaemonRunner`]: watches the configured source roots with a
/// [`NotifyWatcher`], answers scoped query requests on a [`UnixSocketListener`],
/// and debounces re-indexing against the [`SystemClock`].
struct RealDaemon;

impl DaemonRunner for RealDaemon {
    fn serve(
        &self,
        db: &Db,
        sources: &[Box<dyn SessionSource>],
        git: &dyn GitRunner,
    ) -> Result<()> {
        // Warm the index once so the socket serves current data immediately.
        let _ = crate::index::sync(sources, db, git);

        // The query-serving side owns its own connection to the same on-disk
        // index (a rusqlite `Connection` is not `Sync`), so it can run on a
        // background thread while the watch loop re-indexes on this thread.
        let sock_path = resolve_sock_path();
        let listener = UnixSocketListener::bind(&sock_path)?;
        let serve_db =
            Db::open(resolve_db_path().to_str().ok_or_else(|| {
                Error::other("non-utf8 database path while starting the daemon")
            })?)?;
        std::thread::spawn(move || {
            let _ = serve_loop(&listener, &serve_db);
        });

        // Watch the source roots and re-index on each settled burst until the
        // watch backend disconnects.
        let mut watcher = NotifyWatcher::new(&watch_roots(sources))?;
        let mut debouncer = Debouncer::new(DEBOUNCE_MS);
        let clock = SystemClock::default();
        watch_loop(&mut watcher, &mut debouncer, &clock, sources, db, git)
    }
}

/// Real [`McpRunner`]: pumps the MCP handler over the process's stdin/stdout.
struct RealMcp;

impl McpRunner for RealMcp {
    fn serve(&self, db: &Db, engine: &dyn AskEngine) -> Result<()> {
        crate::mcp_shim::serve_stdio(db, engine)
    }
}

/// Build the default set of sources from the environment (Claude Code + Codex).
fn default_sources() -> Vec<Box<dyn SessionSource>> {
    vec![
        Box::new(ClaudeSource::from_env()),
        Box::new(CodexSource::from_env()),
    ]
}

/// Build the `ask` engine from the environment.
///
/// When `ANTHROPIC_API_KEY` is set, wire the real RAG engine (Anthropic chat
/// plus, if `VOYAGE_API_KEY` is also set, Voyage embeddings for hybrid
/// retrieval). Otherwise return [`UnavailableAsk`], which reports that `ask` is
/// not yet configured.
fn default_ask_engine() -> Box<dyn AskEngine> {
    match crate::chat_shim::AnthropicChat::from_env() {
        Ok(chat) => {
            let model = std::env::var("CSX_EMBED_MODEL").unwrap_or_else(|_| "voyage-3".to_string());
            let embedder = crate::embed_shim::VoyageEmbedder::from_env().ok();
            Box::new(crate::rag::RagEngine::new(chat, embedder, model))
        }
        Err(_) => Box::new(UnavailableAsk),
    }
}

/// Parse process arguments, open the real database, and dispatch to stdout.
///
/// This is the only impure entry point; all decision logic is in
/// [`dispatch`](crate::cli::dispatch) and the per-command handlers, which the
/// tests drive directly.
pub fn run() -> Result<()> {
    let cli = Cli::parse();
    let db_path = resolve_db_path();
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let db =
        Db::open(db_path.to_str().ok_or_else(|| {
            Error::other(format!("non-utf8 database path: {}", db_path.display()))
        })?)?;
    let sources = default_sources();
    let git = ProcessGit;
    let engine = default_ask_engine();
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    dispatch(
        cli,
        &db,
        &sources,
        &git,
        engine.as_ref(),
        &RealDaemon,
        &RealMcp,
        &mut lock,
    )
}
