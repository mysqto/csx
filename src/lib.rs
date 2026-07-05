//! csx — fast local full-text and hybrid search over AI-coding session
//! transcripts (Claude Code and Codex CLI), scoped by account, repo, tool,
//! branch, and time.
//!
//! This crate is organized around ports-and-adapters: all real OS/network
//! side effects live behind traits with `*_shim.rs` implementations, while all
//! decision logic lives in unit-testable modules.

#![warn(missing_docs)]

pub mod analyze;
pub mod chat_shim;
pub mod cli;
pub mod cli_shim;
pub mod clock_shim;
pub mod daemon;
pub mod db;
pub mod embed;
pub mod embed_shim;
pub mod error;
pub mod git_shim;
pub mod index;
pub mod mcp;
pub mod mcp_shim;
pub mod model;
pub mod output;
pub mod rag;
pub mod repo;
pub mod source;
pub mod transport_shim;
pub mod watch_shim;

pub use analyze::{
    prompt_hash, run_analyzer, AnalysisStore, AnalyzeStats, Analyzer, Entity, SessionAnalysis,
};
pub use chat_shim::AnthropicChat;
pub use cli::{AskEngine, Cli, Command, ScopeArgs};
pub use cli_shim::run;
pub use clock_shim::SystemClock;
pub use daemon::{
    handle_request, parse_request, serve_loop, serve_once, watch_loop, watch_step, Clock,
    Debouncer, QueryRequest, QueryResponse, RequestScope, ResponseHit,
};
pub use db::{
    CurrentAccount, Db, FileRow, Hit, MessageRow, Scope, SearchOpts, SessionRow, SourceRow,
    SourceSummary,
};
pub use embed::{
    cosine, decode_vec, encode_vec, rank_by_cosine, rrf, rrf_default, Embedder, EmbeddingStore,
    FakeEmbedder, RRF_K,
};
pub use embed_shim::VoyageEmbedder;
pub use error::{Error, Result};
pub use git_shim::{GitRunner, ProcessGit};
pub use index::{sync, SyncStats};
pub use mcp::{
    handle_message, parse_request as parse_mcp_request, tool_catalog, AskArgs, GetSessionArgs,
    Request as McpRequest, Response as McpResponse, RpcError, SearchArgs, INTERNAL_ERROR,
    INVALID_PARAMS, METHOD_NOT_FOUND, PROTOCOL_VERSION, SERVER_NAME, SERVER_VERSION,
};
pub use mcp_shim::{serve_over as serve_mcp_over, serve_stdio as serve_mcp_stdio};
pub use model::{Account, MessageRecord, Role, SessionMeta, Tool};
pub use output::Format;
pub use rag::{
    answer as rag_answer, assemble_prompt, citations, retrieve, to_fts_query, to_passages,
    ChatClient, Passage, RagEngine, SYSTEM_PROMPT,
};
pub use repo::{normalize_remote, resolve_repo_id};
pub use source::{ClaudeSource, CodexSource, ParsedDelta, SessionFile, SessionSource};
pub use transport_shim::{Conn, Listener, UnixConn, UnixSocketListener};
pub use watch_shim::{FileWatcher, NotifyWatcher, WatchEvent};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smoke_reexports() {
        assert_eq!(Tool::ClaudeCode.as_str(), "claude-code");
        assert_eq!(Role::User.as_str(), "user");
        let _e = Error::other("ok");
        assert_eq!(Format::from_json_flag(true), Format::Json);
    }
}
