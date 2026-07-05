//! Real stdio transport for the MCP server.
//!
//! MCP clients speak line-delimited JSON-RPC over a child process's stdin and
//! stdout: one request object per line in, one response object per line out.
//! This module owns that transport — reading lines from stdin and writing
//! response lines to stdout — and pumps each line through the pure handler in
//! [`crate::mcp`].
//!
//! Because it is the only place the crate touches the process's standard
//! streams, it is a `*_shim.rs` and is excluded from coverage. All parsing and
//! dispatch decisions live in [`crate::mcp`] and are unit-tested there; this
//! file contains no branching logic a test would need to reach beyond the raw
//! read/write loop.

use std::io::{BufRead, Write};

use crate::cli::AskEngine;
use crate::db::Db;
use crate::error::Result;
use crate::mcp::{handle_message, parse_request};

/// Serve the MCP protocol over the process's stdin/stdout until stdin reaches
/// end of file.
///
/// Each non-empty input line is parsed and dispatched: a request yields exactly
/// one response line, a notification yields none, and a malformed line yields a
/// JSON-RPC parse-error response. This is a thin wrapper over
/// [`serve_over`] wired to the real standard streams.
pub fn serve_stdio(db: &Db, ask: &dyn AskEngine) -> Result<()> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    serve_over(&mut stdin.lock(), &mut stdout.lock(), db, ask)
}

/// Pump line-delimited JSON-RPC from `input` to `output`, dispatching each line
/// through [`crate::mcp`].
///
/// Kept generic over the reader/writer so `serve_stdio` can wire the real
/// standard streams; the loop itself is transport plumbing and is coverage
/// excluded with the rest of this file.
pub fn serve_over(
    input: &mut dyn BufRead,
    output: &mut dyn Write,
    db: &Db,
    ask: &dyn AskEngine,
) -> Result<()> {
    let mut line = String::new();
    loop {
        line.clear();
        let n = input.read_line(&mut line)?;
        if n == 0 {
            return Ok(());
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let response = match parse_request(trimmed) {
            Ok(req) => match handle_message(db, ask, &req) {
                Some(resp) => resp,
                // A notification: no reply is written.
                None => continue,
            },
            Err(err) => err,
        };
        output.write_all(response.to_line().as_bytes())?;
        output.flush()?;
    }
}
