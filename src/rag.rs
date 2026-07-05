//! Retrieval-augmented generation: hybrid retrieval, cited context assembly,
//! and the ask flow.
//!
//! The pipeline is:
//!
//! 1. Retrieve BM25 hits from [`crate::db::Db::search`] and, when embeddings are
//!    available, a vector-cosine ranking over the aux `embeddings` table; fuse
//!    the two rankings with reciprocal-rank fusion ([`crate::embed::rrf`]).
//! 2. Assemble the fused passages into a numbered, cited context string.
//! 3. Call a [`ChatClient`] with a grounding system prompt and that context.
//! 4. Return the answer text plus the session ids cited as evidence.
//!
//! Every step is a pure function or trait call, so the whole flow is tested
//! with a [`crate::embed::FakeEmbedder`] and a `FakeChatClient` against an
//! in-memory database. The only real [`ChatClient`] (the Anthropic Messages
//! HTTP client) lives in [`crate::chat_shim`] and is excluded from coverage.

use crate::cli::{Answer, AskEngine};
use crate::db::{Db, Hit, Scope, SearchOpts};
use crate::embed::{rank_by_cosine, rrf_default, Embedder, EmbeddingStore};
use crate::error::Result;

/// A chat-completion port.
///
/// Given a `system` prompt and a user `prompt`, produce a single completion
/// string. The real network-backed implementation lives behind a `*_shim.rs`;
/// tests use a `FakeChatClient`.
pub trait ChatClient {
    /// Complete `prompt` under the guidance of `system`, returning the model's
    /// text response.
    fn complete(&self, system: &str, prompt: &str) -> Result<String>;
}

/// One passage of retrieval context: the message body plus the session it came
/// from, tagged with a 1-based citation index.
#[derive(Debug, Clone, PartialEq)]
pub struct Passage {
    /// 1-based citation marker, referenced as `[n]` in the assembled context.
    pub index: usize,
    /// Session the passage was drawn from (the citation target).
    pub session_id: String,
    /// The passage text (a search snippet or message body).
    pub text: String,
}

/// The system prompt that grounds the answer in the retrieved passages.
pub const SYSTEM_PROMPT: &str = "You are a precise assistant answering questions about a developer's own AI-coding session transcripts. Answer only from the numbered context passages below. Cite the passages you use with their bracketed numbers, e.g. [1]. If the context does not contain the answer, say so plainly rather than guessing.";

/// Retrieve the fused top-`limit` passages for `question` under `scope`.
///
/// BM25 hits always participate. When `embedder` is `Some` and the aux
/// `embeddings` table holds vectors for `model`, a vector-cosine ranking is
/// fused in via reciprocal-rank fusion; otherwise the BM25 ranking is used
/// directly. Either way the result is at most `limit` [`Hit`]s in fused order.
pub fn retrieve(
    db: &Db,
    embedder: Option<&dyn Embedder>,
    model: &str,
    question: &str,
    scope: &Scope,
    limit: usize,
) -> Result<Vec<Hit>> {
    // Over-fetch BM25 candidates so fusion has material to reorder, then trim.
    let fetch = limit.saturating_mul(4).max(limit);
    let fts_query = to_fts_query(question);
    let bm25_hits = if fts_query.is_empty() {
        Vec::new()
    } else {
        db.search(
            &fts_query,
            scope,
            &SearchOpts {
                code: false,
                limit: fetch,
            },
        )?
    };

    // Map each hit's session to its best (first-seen) rank position. The BM25
    // ranking, keyed by session id, is the primary ranking.
    let bm25_rank: Vec<String> = dedup_sessions(&bm25_hits);

    // Attempt a vector ranking. It is optional: no embedder, no embeddings for
    // the model, or an embed failure all fall back to BM25-only.
    let vec_rank = match embedder {
        Some(e) => vector_session_ranking(db, e, model, question, &bm25_hits)?,
        None => Vec::new(),
    };

    let fused_sessions: Vec<String> = if vec_rank.is_empty() {
        bm25_rank
    } else {
        rrf_default(&bm25_rank, &vec_rank)
            .into_iter()
            .map(|(s, _)| s)
            .collect()
    };

    // Reassemble hits in fused-session order, one hit per session (the
    // best-ranked message for that session in the BM25 result).
    let mut best_by_session: std::collections::HashMap<&str, &Hit> =
        std::collections::HashMap::new();
    for h in &bm25_hits {
        best_by_session.entry(h.session_id.as_str()).or_insert(h);
    }

    let mut out: Vec<Hit> = Vec::new();
    for sid in fused_sessions {
        if let Some(h) = best_by_session.get(sid.as_str()) {
            out.push((*h).clone());
            if out.len() >= limit {
                break;
            }
        }
    }
    Ok(out)
}

/// Turn a natural-language question into a broad FTS5 `MATCH` query.
///
/// FTS5 treats space-separated bare words as an implicit AND and gives special
/// meaning to characters like `"`, `*`, `(`, `-`, `:`, and `^` — passing a raw
/// question straight through therefore both under-recalls (all words must
/// match) and risks a syntax error. Instead we extract word tokens, keep those
/// of length ≥ 2 (dropping single-letter noise), wrap each in double quotes so
/// FTS treats it as a literal term, and join them with `OR` for broad recall.
/// Returns an empty string when no usable token remains, letting the caller
/// skip the search entirely.
pub fn to_fts_query(question: &str) -> String {
    let terms: Vec<String> = question
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() >= 2)
        .map(|w| format!("\"{}\"", w.to_lowercase()))
        .collect();
    terms.join(" OR ")
}

/// Deduplicate hits to a session-id ranking, preserving first-seen order.
fn dedup_sessions(hits: &[Hit]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for h in hits {
        if seen.insert(h.session_id.clone()) {
            out.push(h.session_id.clone());
        }
    }
    out
}

/// Rank the candidate sessions by the cosine similarity of the question
/// embedding to their message embeddings.
///
/// Candidates are restricted to messages belonging to the BM25 hits' sessions
/// (so the vector pass reorders the lexical candidate pool rather than scanning
/// the whole corpus). Returns a session-id ranking, or an empty vector when no
/// embeddings are available.
fn vector_session_ranking(
    db: &Db,
    embedder: &dyn Embedder,
    model: &str,
    question: &str,
    bm25_hits: &[Hit],
) -> Result<Vec<String>> {
    let store = EmbeddingStore::new(db);
    store.init()?;
    let all = store.load_all(model)?;
    if all.is_empty() {
        return Ok(Vec::new());
    }

    // Which sessions are in play, and the session each embedded message belongs
    // to. We fetch message→session mappings from the messages table.
    let candidate_sessions: std::collections::HashSet<&str> =
        bm25_hits.iter().map(|h| h.session_id.as_str()).collect();
    if candidate_sessions.is_empty() {
        return Ok(Vec::new());
    }

    let msg_session = message_sessions(db)?;
    let candidates: Vec<(i64, Vec<f32>)> = all
        .into_iter()
        .filter(|(id, _)| {
            msg_session
                .get(id)
                .map(|s| candidate_sessions.contains(s.as_str()))
                .unwrap_or(false)
        })
        .collect();
    if candidates.is_empty() {
        return Ok(Vec::new());
    }

    let qvec = embedder.embed(&[question.to_string()])?;
    let qvec = match qvec.into_iter().next() {
        Some(v) => v,
        None => return Ok(Vec::new()),
    };

    let ranked_ids = rank_by_cosine(&qvec, &candidates);

    // Project message ranking onto sessions, first-seen order.
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for id in ranked_ids {
        if let Some(sid) = msg_session.get(&id) {
            if seen.insert(sid.clone()) {
                out.push(sid.clone());
            }
        }
    }
    Ok(out)
}

/// Map every message id to its session id.
fn message_sessions(db: &Db) -> Result<std::collections::HashMap<i64, String>> {
    let mut stmt = db
        .conn()
        .prepare("SELECT id, session_id FROM messages WHERE session_id IS NOT NULL")?;
    let rows = stmt.query_map([], |r| {
        let id: i64 = r.get(0)?;
        let sid: String = r.get(1)?;
        Ok((id, sid))
    })?;
    let mut out = std::collections::HashMap::new();
    for row in rows {
        let (id, sid) = row?;
        out.insert(id, sid);
    }
    Ok(out)
}

/// Turn retrieved hits into numbered, cited passages.
pub fn to_passages(hits: &[Hit]) -> Vec<Passage> {
    hits.iter()
        .enumerate()
        .map(|(i, h)| Passage {
            index: i + 1,
            session_id: h.session_id.clone(),
            text: h.snippet.clone(),
        })
        .collect()
}

/// Assemble the cited context block plus the question into a single user
/// prompt for the chat model.
///
/// Each passage is rendered as `[n] (session <id>): <text>`; the question
/// follows under a `Question:` header. Pure and deterministic — the core of the
/// RAG assembly tested directly.
pub fn assemble_prompt(passages: &[Passage], question: &str) -> String {
    let mut s = String::new();
    s.push_str("Context passages:\n");
    if passages.is_empty() {
        s.push_str("(no matching passages)\n");
    } else {
        for p in passages {
            s.push_str(&format!(
                "[{}] (session {}): {}\n",
                p.index, p.session_id, p.text
            ));
        }
    }
    s.push_str("\nQuestion: ");
    s.push_str(question);
    s
}

/// Distinct session ids across the passages, in order of first appearance.
/// These become the [`Answer::citations`].
pub fn citations(passages: &[Passage]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for p in passages {
        if seen.insert(p.session_id.clone()) {
            out.push(p.session_id.clone());
        }
    }
    out
}

/// The full RAG flow: retrieve, assemble, complete, and return the answer with
/// its citations. This is the tested core; [`RagEngine`] wires it as an
/// [`AskEngine`].
pub fn answer(
    db: &Db,
    embedder: Option<&dyn Embedder>,
    chat: &dyn ChatClient,
    model: &str,
    question: &str,
    scope: &Scope,
    limit: usize,
) -> Result<Answer> {
    let hits = retrieve(db, embedder, model, question, scope, limit)?;
    let passages = to_passages(&hits);
    let prompt = assemble_prompt(&passages, question);
    let text = chat.complete(SYSTEM_PROMPT, &prompt)?;
    Ok(Answer {
        text,
        citations: citations(&passages),
    })
}

/// An [`AskEngine`] backed by the RAG flow: a [`ChatClient`], an optional
/// [`Embedder`], and the embedding model tag used to select stored vectors.
pub struct RagEngine<C: ChatClient, E: Embedder> {
    chat: C,
    embedder: Option<E>,
    model: String,
}

impl<C: ChatClient, E: Embedder> RagEngine<C, E> {
    /// Build a RAG engine. Pass `embedder = None` for BM25-only retrieval;
    /// `model` tags which stored embeddings a vector pass would use.
    pub fn new(chat: C, embedder: Option<E>, model: impl Into<String>) -> Self {
        RagEngine {
            chat,
            embedder,
            model: model.into(),
        }
    }
}

impl<C: ChatClient, E: Embedder> AskEngine for RagEngine<C, E> {
    fn ask(&self, db: &Db, question: &str, scope: &Scope, limit: usize) -> Result<Answer> {
        let embedder = self.embedder.as_ref().map(|e| e as &dyn Embedder);
        answer(
            db,
            embedder,
            &self.chat,
            &self.model,
            question,
            scope,
            limit,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::FakeEmbedder;
    use crate::model::{MessageRecord, Role, SessionMeta, Tool};

    /// A chat client that echoes the prompt it received, so tests can assert on
    /// what the assembly produced. Records the last system/prompt pair.
    struct FakeChatClient {
        reply: String,
        seen: std::cell::RefCell<Option<(String, String)>>,
    }

    impl FakeChatClient {
        fn new(reply: &str) -> Self {
            FakeChatClient {
                reply: reply.to_string(),
                seen: std::cell::RefCell::new(None),
            }
        }
    }

    impl ChatClient for FakeChatClient {
        fn complete(&self, system: &str, prompt: &str) -> Result<String> {
            *self.seen.borrow_mut() = Some((system.to_string(), prompt.to_string()));
            Ok(self.reply.clone())
        }
    }

    fn seed_db() -> Db {
        let db = Db::open(":memory:").unwrap();
        let src = db
            .upsert_source(&crate::db::SourceRow {
                tool: Some("claude-code".into()),
                ..Default::default()
            })
            .unwrap();
        let insert = |db: &Db, session: &str, seq: u64, text: &str, ts: i64| {
            db.upsert_session(
                src,
                &SessionMeta {
                    session_id: session.into(),
                    tool: Tool::ClaudeCode,
                    repo_id: Some("repo-1".into()),
                    project_path: Some("/proj".into()),
                    project_name: Some("proj".into()),
                    git_branch: Some("main".into()),
                    account: None,
                    first_ts: ts,
                    last_ts: ts,
                },
                (seq + 1) as i64,
                None,
            )
            .unwrap();
            db.insert_message(
                src,
                &MessageRecord {
                    session_id: session.into(),
                    tool: Tool::ClaudeCode,
                    seq,
                    ts,
                    role: Role::Assistant,
                    tool_name: None,
                    uuid: None,
                    text: text.into(),
                    cwd: None,
                },
            )
            .unwrap()
        };
        insert(
            &db,
            "s-slow",
            0,
            "the database query is slow because of a missing index",
            100,
        );
        insert(
            &db,
            "s-fast",
            0,
            "the cache made requests fast and snappy",
            200,
        );
        insert(
            &db,
            "s-slow",
            1,
            "we added an index to speed up the slow query",
            300,
        );
        db
    }

    #[test]
    fn assemble_prompt_numbers_and_cites() {
        let passages = vec![
            Passage {
                index: 1,
                session_id: "s1".into(),
                text: "first".into(),
            },
            Passage {
                index: 2,
                session_id: "s2".into(),
                text: "second".into(),
            },
        ];
        let prompt = assemble_prompt(&passages, "why?");
        assert!(prompt.contains("[1] (session s1): first"));
        assert!(prompt.contains("[2] (session s2): second"));
        assert!(prompt.trim_end().ends_with("Question: why?"));
    }

    #[test]
    fn assemble_prompt_handles_no_passages() {
        let prompt = assemble_prompt(&[], "what now?");
        assert!(prompt.contains("(no matching passages)"));
        assert!(prompt.contains("Question: what now?"));
    }

    #[test]
    fn to_passages_indexes_from_one() {
        let hits = vec![
            Hit {
                session_id: "a".into(),
                tool: None,
                repo_id: None,
                project_name: None,
                ts: 1,
                snippet: "x".into(),
                score: 1.0,
            },
            Hit {
                session_id: "b".into(),
                tool: None,
                repo_id: None,
                project_name: None,
                ts: 2,
                snippet: "y".into(),
                score: 0.5,
            },
        ];
        let ps = to_passages(&hits);
        assert_eq!(ps[0].index, 1);
        assert_eq!(ps[1].index, 2);
        assert_eq!(ps[0].session_id, "a");
        assert_eq!(ps[0].text, "x");
    }

    #[test]
    fn citations_dedup_preserving_order() {
        let passages = vec![
            Passage {
                index: 1,
                session_id: "s-slow".into(),
                text: "a".into(),
            },
            Passage {
                index: 2,
                session_id: "s-fast".into(),
                text: "b".into(),
            },
            Passage {
                index: 3,
                session_id: "s-slow".into(),
                text: "c".into(),
            },
        ];
        assert_eq!(citations(&passages), vec!["s-slow", "s-fast"]);
    }

    #[test]
    fn to_fts_query_tokenizes_and_ors() {
        let q = to_fts_query("Why was the query slow?");
        assert_eq!(q, "\"why\" OR \"was\" OR \"the\" OR \"query\" OR \"slow\"");
    }

    #[test]
    fn to_fts_query_drops_single_chars_and_splits_punct() {
        // Single-letter "a" dropped; "co-op" splits on the hyphen into two
        // multi-char tokens.
        let q = to_fts_query("a co-op plan");
        assert_eq!(q, "\"co\" OR \"op\" OR \"plan\"");
    }

    #[test]
    fn to_fts_query_empty_for_no_terms() {
        assert_eq!(to_fts_query(""), "");
        assert_eq!(to_fts_query("? ! . , a"), "");
    }

    #[test]
    fn retrieve_empty_question_returns_no_hits() {
        let db = seed_db();
        let scope = Scope::default();
        let hits = retrieve(&db, None, "voyage", "?!", &scope, 5).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn retrieve_natural_language_question_matches() {
        let db = seed_db();
        let scope = Scope::default();
        let hits = retrieve(&db, None, "voyage", "Why was the query so slow?", &scope, 5).unwrap();
        assert!(hits.iter().any(|h| h.session_id == "s-slow"));
    }

    #[test]
    fn retrieve_bm25_only_finds_scoped_hits() {
        let db = seed_db();
        let scope = Scope::default();
        let hits = retrieve(&db, None, "voyage", "slow query index", &scope, 5).unwrap();
        assert!(!hits.is_empty());
        assert!(hits.iter().any(|h| h.session_id == "s-slow"));
        // One hit per session.
        let uniq: std::collections::HashSet<_> = hits.iter().map(|h| &h.session_id).collect();
        assert_eq!(uniq.len(), hits.len());
    }

    #[test]
    fn retrieve_respects_limit() {
        let db = seed_db();
        let scope = Scope::default();
        let hits = retrieve(&db, None, "voyage", "slow fast query", &scope, 1).unwrap();
        assert!(hits.len() <= 1);
    }

    #[test]
    fn retrieve_with_embeddings_fuses() {
        let db = seed_db();
        let embedder = FakeEmbedder::new(8);
        // Embed each message body and store under model "voyage".
        let store = EmbeddingStore::new(&db);
        store.init().unwrap();
        let msgs = message_sessions(&db).unwrap();
        for &id in msgs.keys() {
            let body: String = db
                .conn()
                .query_row(
                    "SELECT body FROM messages_text WHERE id = ?1",
                    rusqlite::params![id],
                    |r| r.get(0),
                )
                .unwrap();
            let v = embedder.embed(&[body]).unwrap().pop().unwrap();
            store.put(id, "voyage", &v).unwrap();
        }
        let scope = Scope::default();
        let hits = retrieve(
            &db,
            Some(&embedder),
            "voyage",
            "slow query index",
            &scope,
            5,
        )
        .unwrap();
        assert!(!hits.is_empty());
        assert!(hits.iter().any(|h| h.session_id == "s-slow"));
    }

    #[test]
    fn retrieve_embeddings_absent_falls_back_to_bm25() {
        let db = seed_db();
        let embedder = FakeEmbedder::new(8);
        // No vectors stored -> vector ranking empty -> BM25 fallback.
        let scope = Scope::default();
        let hits = retrieve(&db, Some(&embedder), "voyage", "slow query", &scope, 5).unwrap();
        assert!(!hits.is_empty());
    }

    #[test]
    fn answer_flow_produces_text_and_citations() {
        let db = seed_db();
        let chat = FakeChatClient::new("Because an index was missing [1].");
        let scope = Scope::default();
        let ans = answer(
            &db,
            None,
            &chat,
            "voyage",
            "why was the query slow",
            &scope,
            5,
        )
        .unwrap();
        assert_eq!(ans.text, "Because an index was missing [1].");
        assert!(!ans.citations.is_empty());
        // The chat client saw the grounding system prompt and the assembled
        // context.
        let (sys, prompt) = chat.seen.borrow().clone().unwrap();
        assert_eq!(sys, SYSTEM_PROMPT);
        assert!(prompt.contains("Context passages:"));
        assert!(prompt.contains("why was the query slow"));
    }

    #[test]
    fn answer_flow_no_hits_still_answers() {
        let db = seed_db();
        let chat = FakeChatClient::new("I don't have that in the transcripts.");
        let scope = Scope::default();
        let ans = answer(&db, None, &chat, "voyage", "zzzznotawordxyzzy", &scope, 5).unwrap();
        assert_eq!(ans.text, "I don't have that in the transcripts.");
        assert!(ans.citations.is_empty());
        let (_, prompt) = chat.seen.borrow().clone().unwrap();
        assert!(prompt.contains("(no matching passages)"));
    }

    #[test]
    fn rag_engine_implements_ask_engine() {
        let db = seed_db();
        let engine = RagEngine::new(
            FakeChatClient::new("grounded answer [1]"),
            Some(FakeEmbedder::new(8)),
            "voyage",
        );
        let scope = Scope::default();
        let ans = engine.ask(&db, "slow query", &scope, 3).unwrap();
        assert_eq!(ans.text, "grounded answer [1]");
    }

    #[test]
    fn rag_engine_without_embedder() {
        let db = seed_db();
        let engine: RagEngine<FakeChatClient, FakeEmbedder> =
            RagEngine::new(FakeChatClient::new("ok"), None, "voyage");
        let scope = Scope::default();
        let ans = engine.ask(&db, "cache fast", &scope, 3).unwrap();
        assert_eq!(ans.text, "ok");
    }

    /// An embedder that yields an empty batch, to exercise the "no question
    /// vector" guard in the vector ranking.
    struct EmptyEmbedder;
    impl Embedder for EmptyEmbedder {
        fn embed(&self, _texts: &[String]) -> Result<Vec<Vec<f32>>> {
            Ok(Vec::new())
        }
    }

    #[test]
    fn vector_ranking_empty_when_no_bm25_candidates() {
        // Embeddings exist for the model, but the candidate hit set is empty, so
        // the ranking bails at the empty-candidate-sessions guard.
        let db = seed_db();
        let embedder = FakeEmbedder::new(8);
        let store = EmbeddingStore::new(&db);
        store.init().unwrap();
        store
            .put(1, "voyage", &embedder.embed(&["x".into()]).unwrap()[0])
            .unwrap();
        let ranking = vector_session_ranking(&db, &embedder, "voyage", "q", &[]).unwrap();
        assert!(ranking.is_empty());
    }

    #[test]
    fn vector_ranking_empty_when_no_embeddings_stored() {
        // load_all returns empty (nothing stored for the model) -> early return.
        let db = seed_db();
        let embedder = FakeEmbedder::new(8);
        let hits = db
            .search("slow", &Scope::default(), &crate::db::SearchOpts::default())
            .unwrap();
        assert!(!hits.is_empty());
        let ranking = vector_session_ranking(&db, &embedder, "voyage", "q", &hits).unwrap();
        assert!(ranking.is_empty());
    }

    #[test]
    fn vector_ranking_empty_when_candidates_dont_overlap() {
        // Embeddings are stored for message ids that belong to no candidate
        // session, so the filtered candidate list is empty.
        let db = seed_db();
        let embedder = FakeEmbedder::new(8);
        let store = EmbeddingStore::new(&db);
        store.init().unwrap();
        // Store an embedding under a message id that does not exist / is not in
        // any candidate session.
        store
            .put(
                999_999,
                "voyage",
                &embedder.embed(&["x".into()]).unwrap()[0],
            )
            .unwrap();
        let hits = db
            .search("slow", &Scope::default(), &crate::db::SearchOpts::default())
            .unwrap();
        let ranking = vector_session_ranking(&db, &embedder, "voyage", "q", &hits).unwrap();
        assert!(ranking.is_empty());
    }

    #[test]
    fn vector_ranking_empty_when_embedder_yields_nothing() {
        // Real candidates exist, but the embedder returns no question vector.
        let db = seed_db();
        let store = EmbeddingStore::new(&db);
        store.init().unwrap();
        let real = FakeEmbedder::new(8);
        let msgs = message_sessions(&db).unwrap();
        for &id in msgs.keys() {
            store
                .put(id, "voyage", &real.embed(&["b".into()]).unwrap()[0])
                .unwrap();
        }
        let hits = db
            .search("slow", &Scope::default(), &crate::db::SearchOpts::default())
            .unwrap();
        assert!(!hits.is_empty());
        let ranking = vector_session_ranking(&db, &EmptyEmbedder, "voyage", "q", &hits).unwrap();
        assert!(ranking.is_empty());
    }

    #[test]
    fn retrieve_propagates_search_errors() {
        // Dropping the FTS index makes the underlying db.search fail, exercising
        // retrieve's `?` propagation.
        let db = seed_db();
        db.conn().execute_batch("DROP TABLE messages_fts;").unwrap();
        assert!(retrieve(&db, None, "voyage", "slow", &Scope::default(), 5).is_err());
    }
}
