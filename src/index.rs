//! Indexing orchestration.
//!
//! [`sync`] runs a full pass over a set of [`SessionSource`]s: for each source
//! it discovers transcript files, dedups them by `file_key` (a file reachable
//! by several paths is indexed once), skips files whose size and mtime are
//! unchanged since the last pass (the freshness scan), and for the rest parses
//! only the delta beyond the persisted watermark, resolves the session's
//! repository id from its working directory via the [`GitRunner`] port, and
//! upserts the source / session / message rows before advancing the watermark.
//!
//! Everything here is decision logic over the [`Db`], [`SessionSource`], and
//! [`GitRunner`] ports, so it is fully unit-testable with an in-memory database,
//! temp dirs, and a fake git runner — no shim required.

use std::collections::{HashMap, HashSet};

use crate::db::{source_row_from_account, Db};
use crate::error::Result;
use crate::git_shim::GitRunner;
use crate::model::{Account, SessionMeta};
use crate::repo::resolve_repo_id;
use crate::source::{SessionFile, SessionSource};

/// Aggregate outcome of a [`sync`] pass, for reporting and test assertions.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncStats {
    /// Distinct files considered after dedup by `file_key`.
    pub files_seen: usize,
    /// Files skipped because their size and mtime were unchanged.
    pub files_skipped: usize,
    /// Files that were (re)parsed because they were new or had grown.
    pub files_indexed: usize,
    /// New messages inserted across all files in this pass.
    pub messages_added: usize,
    /// Distinct sessions touched (upserted) in this pass.
    pub sessions_touched: usize,
}

/// Run a full indexing pass over every source into `db`.
///
/// Sources are processed in the given order; within each source, discovered
/// files are processed in `file_key`-dedup order. `git` resolves repository
/// identity per working directory (cached across the whole pass). Returns
/// aggregate [`SyncStats`].
pub fn sync(sources: &[Box<dyn SessionSource>], db: &Db, git: &dyn GitRunner) -> Result<SyncStats> {
    let mut stats = SyncStats::default();
    let mut repo_cache: HashMap<String, String> = HashMap::new();
    let mut touched_sessions: HashSet<String> = HashSet::new();

    for source in sources {
        sync_source(
            source.as_ref(),
            db,
            git,
            &mut repo_cache,
            &mut touched_sessions,
            &mut stats,
        )?;
    }

    stats.sessions_touched = touched_sessions.len();
    Ok(stats)
}

/// Index a single source: create its identity row, discover + dedup its files,
/// and fold each fresh/grown file into `db`.
fn sync_source(
    source: &dyn SessionSource,
    db: &Db,
    git: &dyn GitRunner,
    repo_cache: &mut HashMap<String, String>,
    touched_sessions: &mut HashSet<String>,
    stats: &mut SyncStats,
) -> Result<()> {
    let files = dedup_by_file_key(source.discover()?);

    // One identity row per source per pass, built from its account attribution.
    let account = source.account()?.unwrap_or_default();
    let source_id = db.upsert_source(&source_row_from_account(
        source.tool().as_str(),
        source.config_dir().as_deref(),
        None,
        &account,
        Some("cli"),
    ))?;

    for file in &files {
        stats.files_seen += 1;
        index_file(
            source,
            db,
            git,
            source_id,
            &account,
            file,
            repo_cache,
            touched_sessions,
            stats,
        )?;
    }
    Ok(())
}

/// Deduplicate discovered files by `file_key`, keeping the first occurrence
/// (discovery yields a deterministic path order, so this is stable).
fn dedup_by_file_key(files: Vec<SessionFile>) -> Vec<SessionFile> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::with_capacity(files.len());
    for f in files {
        if seen.insert(f.file_key.clone()) {
            out.push(f);
        }
    }
    out
}

/// Fold a single file into the database, honoring the freshness scan and the
/// persisted watermark.
#[allow(clippy::too_many_arguments)]
fn index_file(
    source: &dyn SessionSource,
    db: &Db,
    git: &dyn GitRunner,
    source_id: i64,
    account: &Account,
    file: &SessionFile,
    repo_cache: &mut HashMap<String, String>,
    touched_sessions: &mut HashSet<String>,
    stats: &mut SyncStats,
) -> Result<()> {
    let existing = db.get_file(&file.file_key)?;

    // Freshness scan: an already-tracked file whose size and mtime are both
    // unchanged cannot have grown, so skip it without touching the disk.
    if let Some(row) = &existing {
        if row.size == file.size as i64 && row.mtime == file.mtime {
            stats.files_skipped += 1;
            return Ok(());
        }
    }

    let watermark = existing.as_ref().map(|r| r.watermark).unwrap_or(0);
    let delta = source.parse(file, watermark as u64)?;

    // Resolve repo identity from the session's working directory, if any.
    let mut meta = delta.session;
    if let Some(cwd) = meta.project_path.clone() {
        meta.repo_id = Some(resolve_repo_id(git, &cwd, repo_cache));
    }
    // Prefer the source-level account when the parsed session lacks one.
    if meta.account.is_none() && !is_empty_account(account) {
        meta.account = Some(account.clone());
    }

    let session_id = meta.session_id.clone();
    let added = delta.messages.len();

    // Fold the whole file into the DB atomically. One transaction collapses
    // this file's hundreds of inserts (messages + both FTS indexes) into a
    // single commit — the dominant cost of a cold index — and keeps each
    // file's messages, session rollup, and advanced watermark consistent.
    db.transaction(|| {
        for m in &delta.messages {
            db.insert_message(source_id, m)?;
        }
        let prior = db.session_msg_count(&session_id)?.unwrap_or(0);
        let summary = session_summary(&meta);
        db.upsert_session(source_id, &meta, prior + added as i64, summary.as_deref())?;
        db.upsert_file(
            source_id,
            file.path.to_str(),
            &file.file_key,
            file.size as i64,
            file.mtime,
            Some(&session_id),
        )?;
        db.set_watermark(&file.file_key, delta.new_watermark as i64)
    })?;

    stats.files_indexed += 1;
    stats.messages_added += added;
    touched_sessions.insert(session_id);
    Ok(())
}

/// A summary string for the session row: currently the project name, when
/// known. Kept isolated so later stages can enrich it without touching the
/// upsert flow.
fn session_summary(meta: &SessionMeta) -> Option<String> {
    meta.project_name.clone()
}

/// True when an account carries no attribution at all.
fn is_empty_account(a: &Account) -> bool {
    a.account_uuid.is_none() && a.org_uuid.is_none() && a.email.is_none() && a.org.is_none()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{Error, Result};
    use crate::model::{MessageRecord, Role, Tool};
    use std::cell::RefCell;
    use std::path::PathBuf;

    /// A fake [`GitRunner`] that always resolves the same remote, recording
    /// calls so caching can be asserted.
    #[derive(Default)]
    struct FakeGitRunner {
        calls: RefCell<usize>,
    }

    impl GitRunner for FakeGitRunner {
        fn run(&self, _cwd: &str, args: &[&str]) -> Result<String> {
            *self.calls.borrow_mut() += 1;
            if args == ["config", "--get", "remote.origin.url"] {
                Ok("https://example.com/team/proj.git\n".into())
            } else {
                Err(Error::other("unexpected git call"))
            }
        }
    }

    /// An in-memory fake source over a list of files, each mapping to a fixed
    /// batch of events. On `parse` it re-reads the file's byte length from a
    /// backing map to emulate the watermark/delta contract without touching a
    /// real disk.
    struct FakeSource {
        tool: Tool,
        files: Vec<SessionFile>,
        /// file_key -> ordered (byte_offset_after, messages, session, cwd).
        deltas: HashMap<String, Vec<FakeDelta>>,
        account: Option<Account>,
        /// Account stamped directly onto every parsed session, if any.
        session_account: Option<Account>,
    }

    #[derive(Clone)]
    struct FakeDelta {
        /// Watermark the parse call must be at to yield this delta.
        from: u64,
        /// New watermark after consuming this delta.
        to: u64,
        session_id: String,
        cwd: Option<String>,
        texts: Vec<String>,
    }

    impl FakeSource {
        fn new(tool: Tool) -> Self {
            FakeSource {
                tool,
                files: Vec::new(),
                deltas: HashMap::new(),
                account: None,
                session_account: None,
            }
        }

        fn with_account(mut self, a: Account) -> Self {
            self.account = Some(a);
            self
        }

        fn with_session_account(mut self, a: Account) -> Self {
            self.session_account = Some(a);
            self
        }

        fn add_file(&mut self, key: &str, path: &str, size: u64, mtime: i64) {
            self.files.push(SessionFile {
                path: PathBuf::from(path),
                file_key: key.into(),
                size,
                mtime,
            });
        }

        fn add_delta(
            &mut self,
            key: &str,
            from: u64,
            to: u64,
            session_id: &str,
            cwd: Option<&str>,
            texts: &[&str],
        ) {
            self.deltas.entry(key.into()).or_default().push(FakeDelta {
                from,
                to,
                session_id: session_id.into(),
                cwd: cwd.map(str::to_string),
                texts: texts.iter().map(|s| s.to_string()).collect(),
            });
        }
    }

    impl SessionSource for FakeSource {
        fn tool(&self) -> Tool {
            self.tool
        }

        fn discover(&self) -> Result<Vec<SessionFile>> {
            Ok(self.files.clone())
        }

        fn parse(&self, f: &SessionFile, from_watermark: u64) -> Result<ParsedDeltaShim> {
            let d = self
                .deltas
                .get(&f.file_key)
                .and_then(|ds| ds.iter().find(|d| d.from == from_watermark))
                .cloned()
                .unwrap_or(FakeDelta {
                    from: from_watermark,
                    to: from_watermark,
                    session_id: String::new(),
                    cwd: None,
                    texts: Vec::new(),
                });

            let messages = d
                .texts
                .iter()
                .enumerate()
                .map(|(i, t)| MessageRecord {
                    session_id: d.session_id.clone(),
                    tool: self.tool,
                    seq: i as u64,
                    ts: 1_000 + i as i64,
                    role: Role::User,
                    tool_name: None,
                    uuid: None,
                    text: t.clone(),
                    cwd: d.cwd.clone(),
                })
                .collect();

            Ok(ParsedDeltaShim {
                messages,
                session: SessionMeta {
                    session_id: d.session_id.clone(),
                    tool: self.tool,
                    project_path: d.cwd.clone(),
                    repo_id: None,
                    project_name: d
                        .cwd
                        .as_deref()
                        .map(|c| c.rsplit('/').next().unwrap().into()),
                    git_branch: Some("main".into()),
                    account: self.session_account.clone(),
                    first_ts: 1_000,
                    last_ts: 1_000 + d.texts.len().max(1) as i64,
                },
                new_watermark: d.to,
            })
        }

        fn account(&self) -> Result<Option<Account>> {
            Ok(self.account.clone())
        }

        fn config_dir(&self) -> Option<String> {
            Some("/cfg".into())
        }
    }

    // Alias so the fake's return type reads naturally above.
    use crate::source::ParsedDelta as ParsedDeltaShim;

    fn boxed(s: FakeSource) -> Box<dyn SessionSource> {
        Box::new(s)
    }

    #[test]
    fn initial_index_inserts_everything() {
        let mut src = FakeSource::new(Tool::ClaudeCode).with_account(Account {
            account_uuid: Some("acct".into()),
            org_uuid: None,
            email: Some("dev@example.com".into()),
            org: None,
        });
        src.add_file("k1", "/logs/a.jsonl", 100, 10);
        src.add_delta(
            "k1",
            0,
            100,
            "sess-1",
            Some("/work/proj"),
            &["hello parser", "world"],
        );

        let db = Db::open(":memory:").unwrap();
        let git = FakeGitRunner::default();
        let stats = sync(&[boxed(src)], &db, &git).unwrap();

        assert_eq!(stats.files_seen, 1);
        assert_eq!(stats.files_indexed, 1);
        assert_eq!(stats.files_skipped, 0);
        assert_eq!(stats.messages_added, 2);
        assert_eq!(stats.sessions_touched, 1);

        // Session row carries the resolved repo id and count.
        let (repo, count, branch): (Option<String>, i64, Option<String>) = db
            .conn()
            .query_row(
                "SELECT repo_id, msg_count, git_branch FROM sessions WHERE session_id='sess-1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(repo.as_deref(), Some("example.com/team/proj"));
        assert_eq!(count, 2);
        assert_eq!(branch.as_deref(), Some("main"));

        // Watermark advanced to end of file.
        assert_eq!(db.get_watermark("k1").unwrap(), Some(100));
        // The source-level account was folded into the session.
        let acct_present: i64 = db
            .conn()
            .query_row(
                "SELECT count(*) FROM sources WHERE account_uuid='acct'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(acct_present, 1);
    }

    #[test]
    fn resync_with_no_changes_is_a_noop() {
        let mut src = FakeSource::new(Tool::Codex);
        src.add_file("k1", "/logs/a.jsonl", 100, 10);
        src.add_delta("k1", 0, 100, "sess-1", Some("/work/proj"), &["one", "two"]);

        let db = Db::open(":memory:").unwrap();
        let git = FakeGitRunner::default();
        let first = sync(&[boxed(src)], &db, &git).unwrap();
        assert_eq!(first.messages_added, 2);
        assert_eq!(first.files_indexed, 1);

        // Re-run with an identical source (same size+mtime): nothing re-read.
        let mut src2 = FakeSource::new(Tool::Codex);
        src2.add_file("k1", "/logs/a.jsonl", 100, 10);
        // A delta from 0 exists but must NOT be consulted because the file is
        // fresh; register one that would double-count if wrongly re-read.
        src2.add_delta("k1", 0, 100, "sess-1", Some("/work/proj"), &["one", "two"]);
        let second = sync(&[boxed(src2)], &db, &git).unwrap();

        assert_eq!(second.files_seen, 1);
        assert_eq!(second.files_skipped, 1);
        assert_eq!(second.files_indexed, 0);
        assert_eq!(second.messages_added, 0);

        // Message count did not grow.
        let msgs: i64 = db
            .conn()
            .query_row("SELECT count(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(msgs, 2);
    }

    #[test]
    fn incremental_resync_reads_only_the_delta() {
        let db = Db::open(":memory:").unwrap();
        let git = FakeGitRunner::default();

        // Pass 1: file of size 100, two messages, watermark ends at 100.
        let mut src = FakeSource::new(Tool::ClaudeCode);
        src.add_file("k1", "/logs/a.jsonl", 100, 10);
        src.add_delta("k1", 0, 100, "sess-1", Some("/work/proj"), &["m0", "m1"]);
        let p1 = sync(&[boxed(src)], &db, &git).unwrap();
        assert_eq!(p1.messages_added, 2);

        // Pass 2: file grew to 160 (new mtime). The delta is registered ONLY at
        // from=100 -> if the indexer wrongly restarted at 0 there is no delta
        // and it would add nothing; correct behavior reads the 100..160 delta.
        let mut src2 = FakeSource::new(Tool::ClaudeCode);
        src2.add_file("k1", "/logs/a.jsonl", 160, 20);
        src2.add_delta("k1", 100, 160, "sess-1", Some("/work/proj"), &["m2"]);
        let p2 = sync(&[boxed(src2)], &db, &git).unwrap();

        assert_eq!(p2.files_skipped, 0);
        assert_eq!(p2.files_indexed, 1);
        assert_eq!(p2.messages_added, 1, "only the appended message is read");

        // Cumulative count grew from 2 to 3.
        let count: i64 = db
            .conn()
            .query_row(
                "SELECT msg_count FROM sessions WHERE session_id='sess-1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 3);
        let total_msgs: i64 = db
            .conn()
            .query_row("SELECT count(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(total_msgs, 3);
        assert_eq!(db.get_watermark("k1").unwrap(), Some(160));
    }

    #[test]
    fn dedup_two_paths_one_file_key_indexes_once() {
        let mut src = FakeSource::new(Tool::ClaudeCode);
        // Two distinct paths, identical file_key (a hard/soft link seen twice).
        src.add_file("shared", "/logs/a.jsonl", 50, 10);
        src.add_file("shared", "/logs/link.jsonl", 50, 10);
        src.add_delta(
            "shared",
            0,
            50,
            "sess-1",
            Some("/work/proj"),
            &["only once"],
        );

        let db = Db::open(":memory:").unwrap();
        let git = FakeGitRunner::default();
        let stats = sync(&[boxed(src)], &db, &git).unwrap();

        // Both paths collapse to one considered file.
        assert_eq!(stats.files_seen, 1);
        assert_eq!(stats.files_indexed, 1);
        assert_eq!(stats.messages_added, 1);

        let files: i64 = db
            .conn()
            .query_row("SELECT count(*) FROM files", [], |r| r.get(0))
            .unwrap();
        assert_eq!(files, 1, "one file row despite two paths");
        let msgs: i64 = db
            .conn()
            .query_row("SELECT count(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(msgs, 1);
    }

    #[test]
    fn repo_cache_reused_across_files_and_sources() {
        // Two files sharing one cwd should trigger exactly one git resolution.
        let mut src = FakeSource::new(Tool::ClaudeCode);
        src.add_file("k1", "/logs/a.jsonl", 10, 1);
        src.add_file("k2", "/logs/b.jsonl", 10, 1);
        src.add_delta("k1", 0, 10, "s1", Some("/work/proj"), &["a"]);
        src.add_delta("k2", 0, 10, "s2", Some("/work/proj"), &["b"]);

        let db = Db::open(":memory:").unwrap();
        let git = FakeGitRunner::default();
        let stats = sync(&[boxed(src)], &db, &git).unwrap();
        assert_eq!(stats.sessions_touched, 2);
        // The remote lookup ran once; the second file hit the cache.
        assert_eq!(*git.calls.borrow(), 1);
    }

    #[test]
    fn session_without_cwd_has_no_repo_id() {
        let mut src = FakeSource::new(Tool::Codex);
        src.add_file("k1", "/logs/a.jsonl", 10, 1);
        src.add_delta("k1", 0, 10, "s1", None, &["no cwd here"]);

        let db = Db::open(":memory:").unwrap();
        let git = FakeGitRunner::default();
        sync(&[boxed(src)], &db, &git).unwrap();

        let repo: Option<String> = db
            .conn()
            .query_row(
                "SELECT repo_id FROM sessions WHERE session_id='s1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(repo, None);
        // No cwd -> git never consulted.
        assert_eq!(*git.calls.borrow(), 0);
    }

    #[test]
    fn empty_source_produces_empty_stats() {
        let src = FakeSource::new(Tool::ClaudeCode);
        let db = Db::open(":memory:").unwrap();
        let git = FakeGitRunner::default();
        let stats = sync(&[boxed(src)], &db, &git).unwrap();
        assert_eq!(stats, SyncStats::default());
    }

    #[test]
    fn is_empty_account_detects_blank() {
        assert!(is_empty_account(&Account::default()));
        assert!(!is_empty_account(&Account {
            email: Some("x@y.z".into()),
            ..Default::default()
        }));
    }

    #[test]
    fn parsed_session_account_short_circuits_the_fold() {
        // The parsed session already carries an account, so the source-level
        // account must not overwrite it (the `meta.account.is_none()` guard).
        let mut src = FakeSource::new(Tool::ClaudeCode)
            .with_account(Account {
                email: Some("source@x.io".into()),
                ..Default::default()
            })
            .with_session_account(Account {
                email: Some("session@x.io".into()),
                ..Default::default()
            });
        src.add_file("k1", "/logs/a.jsonl", 10, 1);
        src.add_delta("k1", 0, 10, "s1", Some("/work/proj"), &["hi"]);

        let db = Db::open(":memory:").unwrap();
        let git = FakeGitRunner::default();
        let stats = sync(&[boxed(src)], &db, &git).unwrap();
        assert_eq!(stats.messages_added, 1);
        // The session is stored; the fold left its pre-set account intact. (The
        // session-account value is not persisted on the session row itself, but
        // exercising the short-circuit branch is the point here.)
        let n: i64 = db
            .conn()
            .query_row(
                "SELECT count(*) FROM sessions WHERE session_id='s1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn source_without_account_still_indexes() {
        // A source with no account attribution (e.g. Codex) exercises the
        // `unwrap_or_default` / empty-account branch: the source row is created
        // and messages still land, with no account folded into the session.
        let mut src = FakeSource::new(Tool::Codex);
        src.add_file("k1", "/logs/a.jsonl", 10, 1);
        src.add_delta("k1", 0, 10, "s1", Some("/work/proj"), &["hi"]);

        let db = Db::open(":memory:").unwrap();
        let git = FakeGitRunner::default();
        let stats = sync(&[boxed(src)], &db, &git).unwrap();
        assert_eq!(stats.messages_added, 1);

        // The source row exists with no account attribution.
        let (n, acct): (i64, Option<String>) = db
            .conn()
            .query_row("SELECT count(*), max(account_uuid) FROM sources", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(acct, None);
    }

    #[test]
    fn summary_is_project_name() {
        let meta = SessionMeta {
            session_id: "s".into(),
            tool: Tool::Codex,
            project_path: Some("/a/proj".into()),
            repo_id: None,
            project_name: Some("proj".into()),
            git_branch: None,
            account: None,
            first_ts: 0,
            last_ts: 0,
        };
        assert_eq!(session_summary(&meta).as_deref(), Some("proj"));
    }

    /// A source that fails at a chosen stage, to drive `sync`'s `?` error arms.
    /// `Stage` selects where the failure lands; every earlier stage succeeds.
    #[derive(Clone, Copy)]
    enum Stage {
        Discover,
        Account,
        Parse,
    }

    struct FailingSource(Stage);

    impl SessionSource for FailingSource {
        fn tool(&self) -> Tool {
            Tool::Codex
        }
        fn discover(&self) -> Result<Vec<crate::source::SessionFile>> {
            if matches!(self.0, Stage::Discover) {
                return Err(Error::other("discover failed"));
            }
            Ok(vec![crate::source::SessionFile {
                path: PathBuf::from("/x/a.jsonl"),
                file_key: "fk".into(),
                size: 10,
                mtime: 1,
            }])
        }
        fn parse(&self, _f: &crate::source::SessionFile, _from: u64) -> Result<ParsedDeltaShim> {
            // Only ever reached in the `Parse` stage, where it must fail.
            Err(Error::other("parse failed"))
        }
        fn account(&self) -> Result<Option<crate::model::Account>> {
            if matches!(self.0, Stage::Account) {
                return Err(Error::other("account failed"));
            }
            Ok(None)
        }
        fn config_dir(&self) -> Option<String> {
            None
        }
    }

    #[test]
    fn sync_propagates_source_errors() {
        let git = FakeGitRunner::default();
        for stage in [Stage::Discover, Stage::Account, Stage::Parse] {
            let db = Db::open(":memory:").unwrap();
            let srcs: Vec<Box<dyn SessionSource>> = vec![Box::new(FailingSource(stage))];
            assert!(sync(&srcs, &db, &git).is_err());
        }
    }
}
