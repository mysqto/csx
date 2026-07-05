//! Session sources: discovery and incremental parsing of transcripts.
//!
//! A [`SessionSource`] abstracts over a tool-specific on-disk transcript
//! format. Implementations read files under a root directory (fully testable
//! against a temp dir) and turn them into normalized [`MessageRecord`]s.

use std::path::PathBuf;

use crate::error::Result;
use crate::model::{Account, MessageRecord, SessionMeta, Tool};

pub mod claude;
pub mod codex;
pub mod fileid;
pub mod jsonl;

pub use claude::ClaudeSource;
pub use codex::CodexSource;

/// A discovered transcript file on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionFile {
    /// Absolute path to the transcript file.
    pub path: PathBuf,
    /// Stable, source-scoped key identifying this file (dedupe/watermark key).
    pub file_key: String,
    /// File size in bytes at discovery time.
    pub size: u64,
    /// File modification time (unix seconds).
    pub mtime: i64,
}

/// The result of parsing new content from a transcript file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedDelta {
    /// Newly parsed messages beyond the provided watermark.
    pub messages: Vec<MessageRecord>,
    /// Aggregate metadata for the session.
    pub session: SessionMeta,
    /// Opaque watermark to persist and pass back on the next parse.
    pub new_watermark: u64,
}

/// A tool-specific source of session transcripts.
pub trait SessionSource {
    /// The tool this source produces transcripts for.
    fn tool(&self) -> Tool;

    /// Discover all candidate transcript files for this source.
    fn discover(&self) -> Result<Vec<SessionFile>>;

    /// Parse content of `f` starting after `from_watermark`.
    fn parse(&self, f: &SessionFile, from_watermark: u64) -> Result<ParsedDelta>;

    /// Account attribution for this source, if any.
    ///
    /// The indexer folds this into the `sources` identity row. The default is
    /// no attribution; concrete sources override it (Claude reads
    /// `.claude.json`, Codex has none).
    fn account(&self) -> Result<Option<Account>> {
        Ok(None)
    }

    /// The config directory this source reads from, for the `sources` identity
    /// row. The default is `None`; concrete sources override it with their root.
    fn config_dir(&self) -> Option<String> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Account, Role};

    /// A trivial in-memory fake proving the trait is object-safe and usable.
    struct FakeSource;

    impl SessionSource for FakeSource {
        fn tool(&self) -> Tool {
            Tool::ClaudeCode
        }

        fn discover(&self) -> Result<Vec<SessionFile>> {
            Ok(vec![SessionFile {
                path: PathBuf::from("/tmp/a.jsonl"),
                file_key: "k".into(),
                size: 10,
                mtime: 5,
            }])
        }

        fn parse(&self, f: &SessionFile, from_watermark: u64) -> Result<ParsedDelta> {
            Ok(ParsedDelta {
                messages: vec![MessageRecord {
                    session_id: "s".into(),
                    tool: self.tool(),
                    seq: 0,
                    ts: 1,
                    role: Role::User,
                    tool_name: None,
                    uuid: None,
                    text: "hi".into(),
                    cwd: None,
                }],
                session: SessionMeta {
                    session_id: "s".into(),
                    tool: self.tool(),
                    project_path: None,
                    repo_id: None,
                    project_name: None,
                    git_branch: None,
                    account: Some(Account::default()),
                    first_ts: 1,
                    last_ts: 1,
                },
                new_watermark: from_watermark + f.size,
            })
        }
    }

    #[test]
    fn fake_source_is_object_safe_and_works() {
        let src: Box<dyn SessionSource> = Box::new(FakeSource);
        assert_eq!(src.tool(), Tool::ClaudeCode);
        let files = src.discover().unwrap();
        assert_eq!(files.len(), 1);
        let delta = src.parse(&files[0], 2).unwrap();
        assert_eq!(delta.messages.len(), 1);
        assert_eq!(delta.new_watermark, 12);
        assert_eq!(delta.session.session_id, "s");
    }
}
