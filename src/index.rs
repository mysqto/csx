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
//! A source's [`SessionSource::parser_version`] says what its parser stores.
//! A file read by an older version is stale. csx reads every file of a stale
//! session again from the start, in one transaction that first deletes the
//! session's messages, so readers see either the old rows or the new ones.
//!
//! Everything here is decision logic over the [`Db`], [`SessionSource`], and
//! [`GitRunner`] ports, so it is fully unit-testable with an in-memory database,
//! temp dirs, and a fake git runner — no shim required.

use std::collections::{HashMap, HashSet};

use crate::db::{source_row_from_account, Db, FileRow};
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

    let version = source.parser_version();
    let mut rows = Vec::with_capacity(files.len());
    for file in &files {
        stats.files_seen += 1;
        rows.push(db.get_file(&file.file_key)?);
    }
    let stale = stale_sessions(&rows, version);

    let mut ctx = FileCtx {
        source,
        db,
        git,
        source_id,
        account: &account,
        version,
        repo_cache,
    };
    // Sessions with a stale file are read again first, each with every
    // session it turns out to share files with. `reread` marks the files
    // that were read that way.
    let mut done: HashSet<String> = HashSet::new();
    let mut reread: HashSet<usize> = HashSet::new();
    for sid in &stale {
        if !done.contains(sid) {
            let family = Family {
                files: &files,
                rows: &rows,
            };
            let ids = reindex_family(&mut ctx, &family, sid, &mut reread, touched_sessions, stats)?;
            done.extend(ids);
        }
    }
    for (i, file) in files.iter().enumerate() {
        if !reread.contains(&i) {
            index_file(&mut ctx, file, touched_sessions, stats)?;
        }
    }
    Ok(())
}

/// What indexing one file of a source needs.
struct FileCtx<'a> {
    source: &'a dyn SessionSource,
    db: &'a Db,
    git: &'a dyn GitRunner,
    source_id: i64,
    account: &'a Account,
    version: u32,
    repo_cache: &'a mut HashMap<String, String>,
}

/// The discovered files of a source and their tracked rows, side by side.
struct Family<'a> {
    files: &'a [SessionFile],
    rows: &'a [Option<FileRow>],
}

/// The non-empty session id stored for a tracked file.
fn stored_session(row: &Option<FileRow>) -> Option<&str> {
    row.as_ref()
        .and_then(|r| r.session_id.as_deref())
        .filter(|s| !s.is_empty())
}

/// The session ids, in first-seen order, of the tracked files that a parser
/// older than `version` read. A file with no session id (or an empty one) has
/// no messages worth reading again, so it never makes a session stale.
fn stale_sessions(rows: &[Option<FileRow>], version: u32) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for row in rows {
        let Some(sid) = stored_session(row) else {
            continue;
        };
        let old = row
            .as_ref()
            .is_some_and(|r| r.parser_version < i64::from(version));
        if old && !out.iter().any(|o| o == sid) {
            out.push(sid.to_string());
        }
    }
    out
}

/// Read session `sid` again from the start, in one transaction: every file
/// stored under it, and every file stored under a session id that those
/// files turn out to hold. A transcript can switch session ids, so a file's
/// stored id is not always the id a full read gives. The messages of all
/// those sessions are deleted and stored again. Returns the session ids.
fn reindex_family(
    ctx: &mut FileCtx,
    family: &Family,
    sid: &str,
    reread: &mut HashSet<usize>,
    touched_sessions: &mut HashSet<String>,
    stats: &mut SyncStats,
) -> Result<Vec<String>> {
    let mut ids = vec![sid.to_string()];
    let mut parsed: Vec<(usize, Prepared)> = Vec::new();
    let mut k = 0;
    while k < ids.len() {
        for (i, row) in family.rows.iter().enumerate() {
            if reread.contains(&i) || stored_session(row) != Some(ids[k].as_str()) {
                continue;
            }
            let delta = ctx.source.parse(&family.files[i], 0)?;
            let found = delta.session.session_id.clone();
            if !found.is_empty() && !ids.contains(&found) {
                ids.push(found);
            }
            reread.insert(i);
            parsed.push((i, prepare(ctx, delta, None)));
        }
        k += 1;
    }
    let db = ctx.db;
    let stored = db.transaction(|| {
        for id in &ids {
            db.delete_session_messages(id)?;
        }
        let mut stored = Vec::with_capacity(parsed.len());
        for (i, p) in &parsed {
            stored.push(store(ctx, &family.files[*i], p)?);
        }
        Ok(stored)
    })?;
    for (session_id, added) in stored {
        stats.files_indexed += 1;
        stats.messages_added += added;
        touched_sessions.insert(session_id);
    }
    Ok(ids)
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
/// persisted watermark. The file row is read again here, right before the
/// parse, so a sync that ran meanwhile is not read twice.
fn index_file(
    ctx: &mut FileCtx,
    file: &SessionFile,
    touched_sessions: &mut HashSet<String>,
    stats: &mut SyncStats,
) -> Result<()> {
    let existing = ctx.db.get_file(&file.file_key)?;

    // Freshness scan: an already-tracked file whose size and mtime are both
    // unchanged cannot have grown, so skip it without touching the disk. A
    // file that an older parser read is not skipped, so its version updates.
    if let Some(row) = &existing {
        let current = row.parser_version >= i64::from(ctx.version);
        if current && row.size == file.size as i64 && row.mtime == file.mtime {
            stats.files_skipped += 1;
            return Ok(());
        }
    }

    let watermark = existing.as_ref().map(|r| r.watermark).unwrap_or(0);
    let delta = ctx.source.parse(file, watermark as u64)?;
    let prepared = prepare(ctx, delta, stored_session(&existing));

    // Fold the whole file into the DB atomically. One transaction collapses
    // this file's hundreds of inserts (messages + both FTS indexes) into a
    // single commit — the dominant cost of a cold index — and keeps each
    // file's messages, session rollup, and advanced watermark consistent.
    let db = ctx.db;
    let (session_id, added) = db.transaction(|| store(ctx, file, &prepared))?;

    stats.files_indexed += 1;
    stats.messages_added += added;
    touched_sessions.insert(session_id);
    Ok(())
}

/// A parsed delta made ready to store.
struct Prepared {
    meta: SessionMeta,
    messages: Vec<crate::model::MessageRecord>,
    new_watermark: u64,
}

/// Get a delta ready to store, outside any transaction: resolve the repo id
/// (git runs here), fold in the source account, and, when the parse found no
/// session id (a later part of a Codex file has no `session_meta`), use
/// `known_session`, the id stored for the file.
fn prepare(
    ctx: &mut FileCtx,
    delta: crate::source::ParsedDelta,
    known_session: Option<&str>,
) -> Prepared {
    let mut meta = delta.session;
    let mut messages = delta.messages;
    if meta.session_id.is_empty() {
        if let Some(known) = known_session {
            meta.session_id = known.to_string();
            for m in messages.iter_mut().filter(|m| m.session_id.is_empty()) {
                m.session_id = known.to_string();
            }
        }
    }
    // Resolve repo identity from the session's working directory, if any.
    if let Some(cwd) = meta.project_path.clone() {
        meta.repo_id = Some(resolve_repo_id(ctx.git, &cwd, ctx.repo_cache));
    }
    // Prefer the source-level account when the parsed session lacks one.
    if meta.account.is_none() && !is_empty_account(ctx.account) {
        meta.account = Some(ctx.account.clone());
    }
    Prepared {
        meta,
        messages,
        new_watermark: delta.new_watermark,
    }
}

/// Store one prepared delta of `file`: its messages, the session rollup, the
/// file row, its watermark and the parser version. Returns the session id
/// and the number of messages added. Call it inside a transaction.
fn store(ctx: &FileCtx, file: &SessionFile, p: &Prepared) -> Result<(String, usize)> {
    let db = ctx.db;
    let session_id = p.meta.session_id.clone();
    let added = p.messages.len();
    for m in &p.messages {
        db.insert_message(ctx.source_id, m)?;
    }
    let prior = db.session_msg_count(&session_id)?.unwrap_or(0);
    let summary = session_summary(&p.meta);
    db.upsert_session(
        ctx.source_id,
        &p.meta,
        prior + added as i64,
        summary.as_deref(),
    )?;
    db.upsert_file(
        ctx.source_id,
        file.path.to_str(),
        &file.file_key,
        file.size as i64,
        file.mtime,
        Some(&session_id),
    )?;
    db.set_watermark(&file.file_key, p.new_watermark as i64)?;
    db.set_parser_version(&file.file_key, ctx.version)?;
    Ok((session_id, added))
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
        /// What `parser_version` returns.
        version: u32,
        /// Origin stamped onto every parsed session.
        origin: Option<crate::model::Origin>,
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
                version: 0,
                origin: None,
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
                    origin: self.origin,
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

        fn parser_version(&self) -> u32 {
            self.version
        }
    }

    /// Files of two sessions plus one file with no session id, read by a
    /// parser of `version`.
    fn versioned_source(version: u32, with_new_file: bool) -> FakeSource {
        let mut src = FakeSource::new(Tool::ClaudeCode);
        src.version = version;
        src.origin = (version > 0).then_some(crate::model::Origin::User);
        src.add_file("main", "/p/s1.jsonl", 100, 10);
        src.add_delta("main", 0, 100, "s1", Some("/w"), &["a", "b"]);
        src.add_file("sub", "/p/s1/subagents/x.jsonl", 50, 10);
        src.add_delta("sub", 0, 50, "s1", Some("/w"), &["c"]);
        src.add_file("other", "/p/s2.jsonl", 10, 10);
        src.add_delta("other", 0, 10, "s2", Some("/w"), &["d"]);
        src.add_file("empty", "/p/e.jsonl", 1, 10);
        if with_new_file {
            src.add_file("new", "/p/s1/subagents/y.jsonl", 5, 10);
            src.add_delta("new", 0, 5, "s1", Some("/w"), &["e"]);
        }
        src
    }

    fn count(db: &Db, sql: &str) -> i64 {
        db.conn().query_row(sql, [], |r| r.get(0)).unwrap()
    }

    #[test]
    fn a_newer_parser_reads_stale_sessions_again_in_place() {
        let db = Db::open(":memory:").unwrap();
        let git = FakeGitRunner::default();
        sync(&[boxed(versioned_source(0, false))], &db, &git).unwrap();
        assert_eq!(
            count(&db, "SELECT COUNT(*) FROM messages WHERE session_id='s1'"),
            3
        );
        // The same parser again: everything is fresh.
        let stats = sync(&[boxed(versioned_source(0, false))], &db, &git).unwrap();
        assert_eq!((stats.files_skipped, stats.files_indexed), (4, 0));

        // Version 1: s1 and s2 are read again from the start, not doubled. A
        // new file of s1 joins after the re-read.
        let stats = sync(&[boxed(versioned_source(1, true))], &db, &git).unwrap();
        assert_eq!(stats.files_seen, 5);
        assert_eq!(stats.files_skipped, 0);
        assert_eq!(stats.files_indexed, 5);
        assert_eq!(
            count(&db, "SELECT COUNT(*) FROM messages WHERE session_id='s1'"),
            4
        );
        assert_eq!(
            count(&db, "SELECT msg_count FROM sessions WHERE session_id='s1'"),
            4
        );
        assert_eq!(
            count(&db, "SELECT COUNT(*) FROM messages WHERE session_id='s2'"),
            1
        );
        assert_eq!(
            count(
                &db,
                "SELECT COUNT(*) FROM sessions WHERE origin = 'user' AND session_id <> ''"
            ),
            2
        );
        assert_eq!(
            count(&db, "SELECT COUNT(*) FROM files WHERE parser_version = 1"),
            5
        );
        assert_eq!(
            count(
                &db,
                "SELECT COUNT(*) FROM messages_fts WHERE messages_fts MATCH 'a'"
            ),
            1
        );

        // Version 1 again: nothing to do.
        let stats = sync(&[boxed(versioned_source(1, true))], &db, &git).unwrap();
        assert_eq!((stats.files_skipped, stats.files_indexed), (5, 0));
    }

    #[test]
    fn a_reread_follows_a_transcript_that_switched_session_ids() {
        // Pass 1: the main transcript of S and its subagent file.
        let db = Db::open(":memory:").unwrap();
        let git = FakeGitRunner::default();
        let mut src = FakeSource::new(Tool::ClaudeCode);
        src.add_file("main", "/p/S.jsonl", 100, 10);
        src.add_delta("main", 0, 100, "S", Some("/w"), &["a"]);
        src.add_file("sub", "/p/S/subagents/x.jsonl", 50, 10);
        src.add_delta("sub", 0, 50, "S", Some("/w"), &["c"]);
        sync(&[boxed(src)], &db, &git).unwrap();
        // Pass 2: the main file grew, and the new part carries another
        // session id F, so the file row now says F.
        let mut src = FakeSource::new(Tool::ClaudeCode);
        src.add_file("main", "/p/S.jsonl", 150, 11);
        src.add_delta("main", 100, 150, "F", Some("/w"), &["x"]);
        src.add_file("sub", "/p/S/subagents/x.jsonl", 50, 10);
        sync(&[boxed(src)], &db, &git).unwrap();
        assert_eq!(
            db.get_file("main").unwrap().unwrap().session_id.as_deref(),
            Some("F")
        );
        // Pass 3, a newer parser: the full read of the main file says S.
        let mut src = FakeSource::new(Tool::ClaudeCode);
        src.version = 1;
        src.add_file("main", "/p/S.jsonl", 150, 11);
        src.add_delta("main", 0, 150, "S", Some("/w"), &["a", "x"]);
        src.add_file("sub", "/p/S/subagents/x.jsonl", 50, 10);
        src.add_delta("sub", 0, 50, "S", Some("/w"), &["c"]);
        sync(&[boxed(src)], &db, &git).unwrap();
        assert_eq!(
            count(&db, "SELECT COUNT(*) FROM messages WHERE session_id='S'"),
            3
        );
        assert_eq!(
            count(&db, "SELECT COUNT(*) FROM messages WHERE session_id='F'"),
            0
        );
        assert_eq!(
            count(&db, "SELECT msg_count FROM sessions WHERE session_id='S'"),
            3
        );
    }

    #[test]
    fn a_part_without_a_session_id_joins_the_stored_session() {
        let db = Db::open(":memory:").unwrap();
        let git = FakeGitRunner::default();
        let mut src = FakeSource::new(Tool::Codex);
        src.add_file("k", "/r.jsonl", 100, 10);
        src.add_delta("k", 0, 100, "S", Some("/w"), &["a"]);
        sync(&[boxed(src)], &db, &git).unwrap();
        let mut src = FakeSource::new(Tool::Codex);
        src.add_file("k", "/r.jsonl", 150, 11);
        src.add_delta("k", 100, 150, "", None, &["b"]);
        sync(&[boxed(src)], &db, &git).unwrap();
        assert_eq!(
            count(&db, "SELECT COUNT(*) FROM messages WHERE session_id='S'"),
            2
        );
        assert_eq!(
            count(&db, "SELECT COUNT(*) FROM messages WHERE session_id=''"),
            0
        );
        assert_eq!(
            db.get_file("k").unwrap().unwrap().session_id.as_deref(),
            Some("S")
        );
        // A new file with no session id stays as it is.
        let mut src = FakeSource::new(Tool::Codex);
        src.add_file("e", "/e.jsonl", 1, 1);
        sync(&[boxed(src)], &db, &git).unwrap();
        assert_eq!(
            db.get_file("e").unwrap().unwrap().session_id.as_deref(),
            Some("")
        );
    }

    #[test]
    fn stale_sessions_skip_files_without_a_session() {
        let row = |sid: Option<&str>, v: i64| {
            Some(FileRow {
                id: 1,
                source_id: 1,
                path: None,
                file_key: "k".into(),
                size: 0,
                mtime: 0,
                watermark: 0,
                session_id: sid.map(str::to_string),
                parser_version: v,
            })
        };
        let rows = vec![
            row(Some("a"), 0),
            None,
            row(None, 0),
            row(Some(""), 0),
            row(Some("b"), 1),
            row(Some("a"), 0),
            row(Some("c"), 0),
        ];
        assert_eq!(stale_sessions(&rows, 1), vec!["a", "c"]);
        assert!(stale_sessions(&rows, 0).is_empty());
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
            origin: None,
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
