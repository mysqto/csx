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

use crate::cli::{dispatch, resolve_db_path, AskEngine, Cli, UnavailableAsk};
use crate::db::Db;
use crate::error::{Error, Result};
use crate::git_shim::ProcessGit;
use crate::source::{ClaudeSource, CodexSource, SessionSource};

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
    dispatch(cli, &db, &sources, &git, engine.as_ref(), &mut lock)
}
