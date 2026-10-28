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
    McpRunner, TabsRunner, UnavailableAsk,
};
use crate::daemon::{serve_loop, watch_loop, Debouncer};
use crate::db::Db;
use crate::error::{Error, Result};
use crate::git_shim::{GitRunner, ProcessGit};
use crate::source::{ClaudeSource, CodexSource, DevinSource, SessionSource};
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

/// Real [`TabsRunner`]: builds the OS ports and the agent context, then calls
/// the tabs pipeline or the Alfred installer. Off macOS it lists nothing and
/// refuses the other actions.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
struct RealTabs {
    /// Folder of the index; holds the sync stamp and lock.
    index_dir: Option<std::path::PathBuf>,
    /// This executable, started as `csx sync` for a background refresh.
    exe: Option<std::path::PathBuf>,
}

fn home_from_env() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(std::path::PathBuf::from)
}

#[cfg(target_os = "macos")]
impl RealTabs {
    /// The one place the real OS ports are built.
    fn with_ports<T>(f: impl FnOnce(&crate::tabs::Ports) -> T) -> T {
        use crate::tabs::os_shim::{RealCommand, RealOsa, RealSleeper, RealTty};
        f(&crate::tabs::Ports {
            osa: &RealOsa,
            cmd: &RealCommand,
            tty: &RealTty,
            sleep: &RealSleeper,
        })
    }

    fn backends(flag: Option<String>) -> Vec<Box<dyn crate::tabs::TerminalBackend>> {
        let env = std::env::var("KITTY_LISTEN_ON").ok();
        let tmpdir = std::env::var("TMPDIR").ok();
        crate::tabs::app::backends(crate::cli::resolve_kitty_socket(flag, env), tmpdir)
    }

    /// The agent context from the real environment and the index.
    fn context(&self, db: &Db) -> crate::agent::AgentContext {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or_default();
        crate::cli::tabs_context(
            self.home(),
            db.config_dirs("claude-code").unwrap_or_default(),
            std::env::var_os("CLAUDE_CONFIG_DIR").map(std::path::PathBuf::from),
            std::env::var_os("CODEX_HOME").map(std::path::PathBuf::from),
            std::env::var_os("XDG_DATA_HOME").map(std::path::PathBuf::from),
            now_ms,
        )
    }
}

#[cfg(target_os = "macos")]
impl TabsRunner for RealTabs {
    fn list(
        &self,
        db: &Db,
        opts: &crate::tabs::app::ListOptions,
        kitty_socket: Option<String>,
    ) -> Result<crate::tabs::TabsReport> {
        let ctx = self.context(db);
        let backends = Self::backends(kitty_socket);
        let mut report = Self::with_ports(|ports| {
            crate::tabs::app::list(ports, &backends, &crate::agent::registry(), &ctx, opts)
        });
        report.warnings.extend(crate::refresh::refresh_warning(
            opts.closed.is_some(),
            self.index_dir.as_deref(),
            self.exe.as_deref(),
            std::time::SystemTime::now(),
            &crate::refresh_shim::RealSpawner,
        ));
        crate::tabs::closed::add_closed(&mut report, db, opts, &ctx.home);
        Ok(report)
    }

    fn focus(&self, tab_ref: &str, kitty_socket: Option<String>) -> Result<()> {
        let backends = Self::backends(kitty_socket);
        Self::with_ports(|ports| crate::tabs::app::focus(ports, &backends, tab_ref))
    }

    fn open(
        &self,
        db: &Db,
        terminal: &str,
        tool: &str,
        session_id: &str,
        kitty_socket: Option<String>,
    ) -> Result<crate::tabs::open::OpenOutcome> {
        let ctx = self.context(db);
        let backends = Self::backends(kitty_socket);
        Self::with_ports(|ports| {
            crate::tabs::open::open_session(
                ports,
                &backends,
                &crate::agent::registry(),
                &ctx,
                db,
                terminal,
                tool,
                session_id,
            )
        })
    }

    fn terminals(&self) -> Result<Vec<&'static crate::tabs::TerminalInfo>> {
        Ok(crate::tabs::installed_terminals(
            &crate::tabs::os_shim::RealCommand,
        ))
    }

    fn alfred_install(
        &self,
        prefs: Option<std::path::PathBuf>,
        hotkeys: &crate::alfred::Hotkeys,
    ) -> Result<crate::alfred::InstallReport> {
        let dir = crate::alfred::resolve_prefs(
            prefs,
            &crate::tabs::os_shim::RealCommand,
            self.home().as_deref(),
        )?;
        crate::alfred::install(&dir, hotkeys)
    }

    fn home(&self) -> Option<std::path::PathBuf> {
        home_from_env()
    }
}

#[cfg(not(target_os = "macos"))]
impl TabsRunner for RealTabs {
    fn list(
        &self,
        _db: &Db,
        _opts: &crate::tabs::app::ListOptions,
        _kitty_socket: Option<String>,
    ) -> Result<crate::tabs::TabsReport> {
        Ok(crate::tabs::TabsReport::default())
    }

    fn focus(&self, _tab_ref: &str, _kitty_socket: Option<String>) -> Result<()> {
        Err(Error::other("csx tabs is supported on macOS only"))
    }

    fn open(
        &self,
        _db: &Db,
        _terminal: &str,
        _tool: &str,
        _session_id: &str,
        _kitty_socket: Option<String>,
    ) -> Result<crate::tabs::open::OpenOutcome> {
        Err(Error::other("csx tabs is supported on macOS only"))
    }

    fn terminals(&self) -> Result<Vec<&'static crate::tabs::TerminalInfo>> {
        Err(Error::other("csx tabs is supported on macOS only"))
    }

    fn alfred_install(
        &self,
        _prefs: Option<std::path::PathBuf>,
        _hotkeys: &crate::alfred::Hotkeys,
    ) -> Result<crate::alfred::InstallReport> {
        Err(Error::other("csx tabs is supported on macOS only"))
    }

    fn home(&self) -> Option<std::path::PathBuf> {
        home_from_env()
    }
}

/// Build the default set of sources from the environment (Claude Code, Codex
/// and Devin).
fn default_sources() -> Vec<Box<dyn SessionSource>> {
    vec![
        Box::new(ClaudeSource::from_env()),
        Box::new(CodexSource::from_env()),
        Box::new(DevinSource::from_env()),
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
        &RealTabs {
            index_dir: db_path.parent().map(std::path::Path::to_path_buf),
            exe: std::env::current_exe().ok(),
        },
        &mut lock,
    )
}
