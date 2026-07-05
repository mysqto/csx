//! Session analysis: versioned summaries and extracted entities.
//!
//! An [`Analyzer`] reads a session's messages and produces a [`SessionAnalysis`]
//! — a one-paragraph summary plus a set of typed entities (files touched, error
//! strings, topics, …). Every analysis is versioned by the pair
//! `(model, prompt_hash)`: the model that produced it and a hash of the exact
//! prompt used. That lets the same session be re-analyzed under a new prompt or
//! model without clobbering prior rows, and lets the [`run_analyzer`] runner
//! skip sessions already analyzed at the current version.
//!
//! Results land in two aux tables:
//!
//! * `session_summaries(session_id, model, prompt_hash, summary,
//!   PRIMARY KEY(session_id, model, prompt_hash))`
//! * `entities(session_id, model, prompt_hash, kind, value)`
//!
//! The [`Analyzer`] port is consumed via a trait so the runner is tested with a
//! deterministic fake against an in-memory database; any real, network-backed
//! analyzer (e.g. one calling a chat model) would be constructed from a
//! `*_shim.rs` client but the runner logic itself stays here and fully tested.

use rusqlite::params;
use sha2::{Digest, Sha256};

use crate::db::{Db, MessageRow};
use crate::error::Result;

/// A single extracted entity: a typed key/value pair (e.g. `kind = "file"`,
/// `value = "src/main.rs"`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entity {
    /// Entity category (`file`, `error`, `topic`, …).
    pub kind: String,
    /// Entity value.
    pub value: String,
}

/// The output of an [`Analyzer`] for one session.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SessionAnalysis {
    /// A short natural-language summary of the session.
    pub summary: String,
    /// Entities extracted from the session.
    pub entities: Vec<Entity>,
}

/// A versioned session analyzer.
///
/// Implementations declare their identity via [`Analyzer::model`] and
/// [`Analyzer::prompt`]; the runner derives the `prompt_hash` from the prompt
/// so a prompt change produces a distinct version. [`Analyzer::analyze`] turns a
/// session's messages into a [`SessionAnalysis`].
pub trait Analyzer {
    /// The model identifier this analyzer is versioned under.
    fn model(&self) -> &str;

    /// The exact prompt this analyzer uses. Hashed into the `prompt_hash`
    /// version component, so any change re-versions the analyzer.
    fn prompt(&self) -> &str;

    /// Analyze one session's messages into a summary + entities.
    fn analyze(&self, session_id: &str, messages: &[MessageRow]) -> Result<SessionAnalysis>;
}

/// Compute the stable `prompt_hash` for a prompt string (lowercase hex SHA-256,
/// truncated to 16 hex chars for compactness).
pub fn prompt_hash(prompt: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(prompt.as_bytes());
    let digest = hasher.finalize();
    let mut s = String::with_capacity(16);
    for byte in digest.iter().take(8) {
        s.push_str(&format!("{byte:02x}"));
    }
    s
}

/// The aux tables for analysis output. Kept separate from the core schema so
/// analysis is an optional layer over an already-indexed database.
const ANALYZE_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS session_summaries (
    session_id  TEXT,
    model       TEXT,
    prompt_hash TEXT,
    summary     TEXT,
    PRIMARY KEY (session_id, model, prompt_hash)
);

CREATE TABLE IF NOT EXISTS entities (
    id          INTEGER PRIMARY KEY,
    session_id  TEXT,
    model       TEXT,
    prompt_hash TEXT,
    kind        TEXT,
    value       TEXT
);
CREATE INDEX IF NOT EXISTS entities_scope
    ON entities(session_id, model, prompt_hash);
CREATE UNIQUE INDEX IF NOT EXISTS entities_uniq
    ON entities(session_id, model, prompt_hash, kind, value);
"#;

/// Store for versioned session analyses over the aux tables.
pub struct AnalysisStore<'a> {
    db: &'a Db,
}

impl<'a> AnalysisStore<'a> {
    /// Wrap a database handle. Call [`AnalysisStore::init`] before use.
    pub fn new(db: &'a Db) -> Self {
        AnalysisStore { db }
    }

    /// Create the analysis tables if absent. Idempotent.
    pub fn init(&self) -> Result<()> {
        self.db.conn().execute_batch(ANALYZE_SCHEMA)?;
        Ok(())
    }

    /// True if a summary already exists for this `(session_id, model,
    /// prompt_hash)` version.
    pub fn has_summary(&self, session_id: &str, model: &str, prompt_hash: &str) -> Result<bool> {
        let n: i64 = self.db.conn().query_row(
            "SELECT COUNT(*) FROM session_summaries
             WHERE session_id = ?1 AND model = ?2 AND prompt_hash = ?3",
            params![session_id, model, prompt_hash],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// Upsert a summary and (re)write its entities for one version.
    ///
    /// The summary row is replaced in place on the `(session_id, model,
    /// prompt_hash)` primary key. Entities for that version are cleared and
    /// re-inserted, so re-running an analyzer at the same version is idempotent
    /// rather than accumulating duplicate entity rows.
    pub fn upsert(
        &self,
        session_id: &str,
        model: &str,
        prompt_hash: &str,
        analysis: &SessionAnalysis,
    ) -> Result<()> {
        self.db.conn().execute(
            "INSERT INTO session_summaries (session_id, model, prompt_hash, summary)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(session_id, model, prompt_hash)
             DO UPDATE SET summary = excluded.summary",
            params![session_id, model, prompt_hash, analysis.summary],
        )?;
        self.db.conn().execute(
            "DELETE FROM entities
             WHERE session_id = ?1 AND model = ?2 AND prompt_hash = ?3",
            params![session_id, model, prompt_hash],
        )?;
        for e in &analysis.entities {
            self.db.conn().execute(
                "INSERT OR IGNORE INTO entities
                    (session_id, model, prompt_hash, kind, value)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![session_id, model, prompt_hash, e.kind, e.value],
            )?;
        }
        Ok(())
    }

    /// Read back the summary for a version, if present.
    pub fn get_summary(
        &self,
        session_id: &str,
        model: &str,
        prompt_hash: &str,
    ) -> Result<Option<String>> {
        use rusqlite::OptionalExtension;
        let s: Option<String> = self
            .db
            .conn()
            .query_row(
                "SELECT summary FROM session_summaries
                 WHERE session_id = ?1 AND model = ?2 AND prompt_hash = ?3",
                params![session_id, model, prompt_hash],
                |r| r.get(0),
            )
            .optional()?;
        Ok(s)
    }

    /// Read back the entities for a version, ordered by `kind` then `value`.
    pub fn get_entities(
        &self,
        session_id: &str,
        model: &str,
        prompt_hash: &str,
    ) -> Result<Vec<Entity>> {
        let mut stmt = self.db.conn().prepare(
            "SELECT kind, value FROM entities
             WHERE session_id = ?1 AND model = ?2 AND prompt_hash = ?3
             ORDER BY kind, value",
        )?;
        let rows = stmt.query_map(params![session_id, model, prompt_hash], |r| {
            Ok(Entity {
                kind: r.get(0)?,
                value: r.get(1)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }
}

/// Outcome of a [`run_analyzer`] pass.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AnalyzeStats {
    /// Sessions newly analyzed on this pass.
    pub analyzed: usize,
    /// Sessions skipped because they were already analyzed at this version.
    pub skipped: usize,
}

/// List every session id known to the database, ordered for determinism.
fn all_session_ids(db: &Db) -> Result<Vec<String>> {
    let mut stmt = db
        .conn()
        .prepare("SELECT session_id FROM sessions ORDER BY session_id")?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Apply `analyzer` to every session not yet analyzed at its current version.
///
/// The version is `(analyzer.model(), prompt_hash(analyzer.prompt()))`. Sessions
/// already carrying a summary at that version are skipped; the rest are analyzed
/// and their summary + entities upserted. Returns per-pass counts so a caller
/// can report progress and confirm the incremental behavior.
pub fn run_analyzer(db: &Db, analyzer: &dyn Analyzer) -> Result<AnalyzeStats> {
    let store = AnalysisStore::new(db);
    store.init()?;
    let model = analyzer.model().to_string();
    let phash = prompt_hash(analyzer.prompt());

    let mut stats = AnalyzeStats::default();
    for sid in all_session_ids(db)? {
        if store.has_summary(&sid, &model, &phash)? {
            stats.skipped += 1;
            continue;
        }
        let messages = db.session_messages(&sid)?;
        let analysis = analyzer.analyze(&sid, &messages)?;
        store.upsert(&sid, &model, &phash, &analysis)?;
        stats.analyzed += 1;
    }
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{MessageRecord, Role, SessionMeta, Tool};

    /// A deterministic analyzer: the summary is the count of messages, and each
    /// message body becomes a `topic` entity. Model/prompt are configurable so
    /// tests can exercise versioning.
    struct FakeAnalyzer {
        model: String,
        prompt: String,
    }

    impl FakeAnalyzer {
        fn new(model: &str, prompt: &str) -> Self {
            FakeAnalyzer {
                model: model.into(),
                prompt: prompt.into(),
            }
        }
    }

    impl Analyzer for FakeAnalyzer {
        fn model(&self) -> &str {
            &self.model
        }
        fn prompt(&self) -> &str {
            &self.prompt
        }
        fn analyze(&self, _session_id: &str, messages: &[MessageRow]) -> Result<SessionAnalysis> {
            let entities = messages
                .iter()
                .map(|m| Entity {
                    kind: "topic".into(),
                    value: m.body.clone(),
                })
                .collect();
            Ok(SessionAnalysis {
                summary: format!("{} messages", messages.len()),
                entities,
            })
        }
    }

    fn seed(db: &Db, session: &str, bodies: &[&str]) {
        let src = db
            .upsert_source(&crate::db::SourceRow {
                tool: Some("claude-code".into()),
                ..Default::default()
            })
            .unwrap();
        db.upsert_session(
            src,
            &SessionMeta {
                session_id: session.into(),
                tool: Tool::ClaudeCode,
                project_path: None,
                repo_id: None,
                project_name: None,
                git_branch: None,
                account: None,
                first_ts: 1,
                last_ts: 1,
            },
            bodies.len() as i64,
            None,
        )
        .unwrap();
        for (i, b) in bodies.iter().enumerate() {
            db.insert_message(
                src,
                &MessageRecord {
                    session_id: session.into(),
                    tool: Tool::ClaudeCode,
                    seq: i as u64,
                    ts: 1 + i as i64,
                    role: Role::User,
                    tool_name: None,
                    uuid: None,
                    text: (*b).into(),
                    cwd: None,
                },
            )
            .unwrap();
        }
    }

    #[test]
    fn prompt_hash_is_stable_and_sensitive() {
        let a = prompt_hash("summarize this");
        let b = prompt_hash("summarize this");
        let c = prompt_hash("summarize THIS");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), 16);
        assert!(a.chars().all(|ch| ch.is_ascii_hexdigit()));
    }

    #[test]
    fn store_init_idempotent_and_round_trip() {
        let db = Db::open(":memory:").unwrap();
        let store = AnalysisStore::new(&db);
        store.init().unwrap();
        store.init().unwrap();

        assert!(!store.has_summary("s1", "m", "h").unwrap());
        assert_eq!(store.get_summary("s1", "m", "h").unwrap(), None);
        assert!(store.get_entities("s1", "m", "h").unwrap().is_empty());

        let analysis = SessionAnalysis {
            summary: "did stuff".into(),
            entities: vec![
                Entity {
                    kind: "file".into(),
                    value: "a.rs".into(),
                },
                Entity {
                    kind: "topic".into(),
                    value: "indexing".into(),
                },
            ],
        };
        store.upsert("s1", "m", "h", &analysis).unwrap();
        assert!(store.has_summary("s1", "m", "h").unwrap());
        assert_eq!(
            store.get_summary("s1", "m", "h").unwrap(),
            Some("did stuff".into())
        );
        let ents = store.get_entities("s1", "m", "h").unwrap();
        assert_eq!(ents.len(), 2);
        assert_eq!(ents[0].kind, "file"); // ordered by kind
        assert_eq!(ents[1].kind, "topic");
    }

    #[test]
    fn upsert_replaces_summary_and_entities_in_place() {
        let db = Db::open(":memory:").unwrap();
        let store = AnalysisStore::new(&db);
        store.init().unwrap();

        store
            .upsert(
                "s1",
                "m",
                "h",
                &SessionAnalysis {
                    summary: "first".into(),
                    entities: vec![Entity {
                        kind: "topic".into(),
                        value: "old".into(),
                    }],
                },
            )
            .unwrap();
        store
            .upsert(
                "s1",
                "m",
                "h",
                &SessionAnalysis {
                    summary: "second".into(),
                    entities: vec![Entity {
                        kind: "topic".into(),
                        value: "new".into(),
                    }],
                },
            )
            .unwrap();

        assert_eq!(
            store.get_summary("s1", "m", "h").unwrap(),
            Some("second".into())
        );
        let ents = store.get_entities("s1", "m", "h").unwrap();
        assert_eq!(ents.len(), 1, "old entities cleared, not accumulated");
        assert_eq!(ents[0].value, "new");
    }

    #[test]
    fn versions_coexist_by_model_and_prompt_hash() {
        let db = Db::open(":memory:").unwrap();
        let store = AnalysisStore::new(&db);
        store.init().unwrap();
        let base = SessionAnalysis {
            summary: "s".into(),
            entities: vec![],
        };
        store.upsert("s1", "m1", "h1", &base).unwrap();
        store.upsert("s1", "m2", "h1", &base).unwrap();
        store.upsert("s1", "m1", "h2", &base).unwrap();

        assert!(store.has_summary("s1", "m1", "h1").unwrap());
        assert!(store.has_summary("s1", "m2", "h1").unwrap());
        assert!(store.has_summary("s1", "m1", "h2").unwrap());
        assert!(!store.has_summary("s1", "m2", "h2").unwrap());
    }

    #[test]
    fn runner_analyzes_all_then_skips_on_rerun() {
        let db = Db::open(":memory:").unwrap();
        seed(&db, "s1", &["hello", "world"]);
        seed(&db, "s2", &["foo"]);

        let analyzer = FakeAnalyzer::new("fake-model", "summarize");
        let first = run_analyzer(&db, &analyzer).unwrap();
        assert_eq!(first.analyzed, 2);
        assert_eq!(first.skipped, 0);

        // Verify persisted output.
        let store = AnalysisStore::new(&db);
        let phash = prompt_hash("summarize");
        assert_eq!(
            store.get_summary("s1", "fake-model", &phash).unwrap(),
            Some("2 messages".into())
        );
        assert_eq!(
            store
                .get_entities("s1", "fake-model", &phash)
                .unwrap()
                .len(),
            2
        );

        // Re-run: everything already at this version -> all skipped.
        let second = run_analyzer(&db, &analyzer).unwrap();
        assert_eq!(second.analyzed, 0);
        assert_eq!(second.skipped, 2);
    }

    #[test]
    fn runner_reanalyzes_new_version() {
        let db = Db::open(":memory:").unwrap();
        seed(&db, "s1", &["a"]);

        run_analyzer(&db, &FakeAnalyzer::new("m", "prompt-v1")).unwrap();
        // A changed prompt is a new version, so the session is analyzed again.
        let stats = run_analyzer(&db, &FakeAnalyzer::new("m", "prompt-v2")).unwrap();
        assert_eq!(stats.analyzed, 1);
        assert_eq!(stats.skipped, 0);

        // Both versions coexist.
        let store = AnalysisStore::new(&db);
        assert!(store
            .has_summary("s1", "m", &prompt_hash("prompt-v1"))
            .unwrap());
        assert!(store
            .has_summary("s1", "m", &prompt_hash("prompt-v2"))
            .unwrap());
    }

    #[test]
    fn runner_on_empty_db_is_noop() {
        let db = Db::open(":memory:").unwrap();
        let stats = run_analyzer(&db, &FakeAnalyzer::new("m", "p")).unwrap();
        assert_eq!(stats, AnalyzeStats::default());
    }

    #[test]
    fn store_methods_report_sql_errors() {
        // Without init() the analysis tables do not exist, so every SQL call
        // takes its `?` error arm.
        let db = Db::open(":memory:").unwrap();
        let store = AnalysisStore::new(&db);
        assert!(store.has_summary("s", "m", "p").is_err());
        let analysis = SessionAnalysis {
            summary: "sum".into(),
            entities: vec![Entity {
                kind: "file".into(),
                value: "a.rs".into(),
            }],
        };
        assert!(store.upsert("s", "m", "p", &analysis).is_err());
        assert!(store.get_summary("s", "m", "p").is_err());
        assert!(store.get_entities("s", "m", "p").is_err());
    }

    #[test]
    fn upsert_entity_insert_error_propagates() {
        // With only the summaries table present, the summary insert and entity
        // delete succeed but inserting an entity fails, covering that `?` arm.
        let db = Db::open(":memory:").unwrap();
        db.conn()
            .execute_batch(
                "CREATE TABLE session_summaries (
                     session_id TEXT, model TEXT, prompt_hash TEXT, summary TEXT,
                     PRIMARY KEY (session_id, model, prompt_hash));
                 CREATE TABLE entities (session_id TEXT, model TEXT, prompt_hash TEXT);",
            )
            .unwrap();
        let store = AnalysisStore::new(&db);
        let analysis = SessionAnalysis {
            summary: "sum".into(),
            entities: vec![Entity {
                kind: "file".into(),
                value: "a.rs".into(),
            }],
        };
        // The entities table is missing the kind/value columns, so the entity
        // insert errors.
        assert!(store.upsert("s", "m", "p", &analysis).is_err());
    }
}
