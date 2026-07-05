//! Embeddings, vector similarity, and hybrid rank fusion.
//!
//! This module owns everything about dense retrieval that a test can reach
//! without touching the network:
//!
//! * the [`Embedder`] port (a batch text→vector function), consumed via a trait
//!   so the RAG and analysis flows can run against a deterministic
//!   [`FakeEmbedder`];
//! * pure math — [`cosine`] similarity and reciprocal-rank fusion ([`rrf`]) —
//!   that blends the BM25 ranking from [`crate::db::Db::search`] with a
//!   vector-cosine ranking;
//! * an [`EmbeddingStore`] over the aux `embeddings` table, where vectors are
//!   persisted as little-endian `f32` BLOBs and read back by message id.
//!
//! The only real [`Embedder`] implementation (the Voyage HTTP client) lives in
//! [`crate::embed_shim`] and is excluded from coverage; every decision here is
//! unit-tested with fakes and an in-memory SQLite database.

use rusqlite::{params, OptionalExtension};

use crate::db::Db;
use crate::error::{Error, Result};

/// A batch text-embedding port.
///
/// Implementations map each input string to a dense vector; all vectors in one
/// call share the same dimensionality. The real network-backed implementation
/// lives behind a `*_shim.rs`; tests use a [`FakeEmbedder`].
pub trait Embedder {
    /// Embed `texts`, returning one vector per input in the same order.
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
}

/// Cosine similarity of two equal-length vectors.
///
/// Returns a value in `[-1.0, 1.0]`. Two guards keep the function total:
/// vectors of differing length return `0.0` (they are not comparable), and a
/// zero-magnitude vector returns `0.0` (avoids a division by zero rather than
/// yielding `NaN`).
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

/// Encode a vector as little-endian `f32` bytes for BLOB storage.
pub fn encode_vec(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for x in v {
        out.extend_from_slice(&x.to_le_bytes());
    }
    out
}

/// Decode a little-endian `f32` BLOB back into a vector.
///
/// Fails if the byte length is not a multiple of four (a truncated or corrupt
/// BLOB), rather than silently dropping the trailing bytes.
pub fn decode_vec(bytes: &[u8]) -> Result<Vec<f32>> {
    if !bytes.len().is_multiple_of(4) {
        return Err(Error::other(format!(
            "embedding blob length {} is not a multiple of 4",
            bytes.len()
        )));
    }
    let mut out = Vec::with_capacity(bytes.len() / 4);
    for chunk in bytes.chunks_exact(4) {
        let arr: [u8; 4] = chunk.try_into().expect("chunk is exactly 4 bytes");
        out.push(f32::from_le_bytes(arr));
    }
    Ok(out)
}

/// Reciprocal-rank fusion of two rankings identified by a common key type.
///
/// Each input is a ranking in descending relevance order (best first). An item
/// at 0-based position `i` in a list contributes `1 / (k + i + 1)` to its fused
/// score; scores from both lists are summed. `k` is the standard RRF damping
/// constant (60 is the common default; see [`rrf_default`]). The result is
/// sorted by fused score descending, with ties broken by the key's own
/// ordering so the fusion is deterministic.
///
/// Keys present in only one list still rank — they simply accrue a single
/// contribution.
pub fn rrf<K>(list_a: &[K], list_b: &[K], k: f64) -> Vec<(K, f64)>
where
    K: Clone + Eq + std::hash::Hash + Ord,
{
    use std::collections::HashMap;

    let mut scores: HashMap<K, f64> = HashMap::new();
    for list in [list_a, list_b] {
        for (i, key) in list.iter().enumerate() {
            let contrib = 1.0 / (k + (i as f64) + 1.0);
            *scores.entry(key.clone()).or_insert(0.0) += contrib;
        }
    }
    let mut fused: Vec<(K, f64)> = scores.into_iter().collect();
    fused.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.0.cmp(&b.0))
    });
    fused
}

/// The conventional RRF damping constant.
pub const RRF_K: f64 = 60.0;

/// Reciprocal-rank fusion with the conventional damping constant [`RRF_K`].
pub fn rrf_default<K>(list_a: &[K], list_b: &[K]) -> Vec<(K, f64)>
where
    K: Clone + Eq + std::hash::Hash + Ord,
{
    rrf(list_a, list_b, RRF_K)
}

/// Rank candidate `(id, vector)` pairs by cosine similarity to `query`,
/// descending. Ties are broken by id so the ordering is deterministic. This is
/// the vector-cosine ranking that [`rrf`] fuses with the BM25 ranking.
pub fn rank_by_cosine(query: &[f32], candidates: &[(i64, Vec<f32>)]) -> Vec<i64> {
    let mut scored: Vec<(i64, f32)> = candidates
        .iter()
        .map(|(id, v)| (*id, cosine(query, v)))
        .collect();
    scored.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.0.cmp(&b.0))
    });
    scored.into_iter().map(|(id, _)| id).collect()
}

/// The aux `embeddings` table: one dense vector per message id, tagged with the
/// model that produced it. Kept separate from the core schema so embedding is
/// an optional layer over an already-indexed database.
const EMBEDDINGS_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS embeddings (
    message_id INTEGER PRIMARY KEY,
    model      TEXT,
    vec        BLOB
);
"#;

/// Store and load embedding vectors keyed by message id.
///
/// Vectors are persisted as little-endian `f32` BLOBs in the aux `embeddings`
/// table. The store borrows a [`Db`] and shares its connection; call
/// [`EmbeddingStore::init`] once to create the table.
pub struct EmbeddingStore<'a> {
    db: &'a Db,
}

impl<'a> EmbeddingStore<'a> {
    /// Wrap a database handle. Does not create the table — call
    /// [`EmbeddingStore::init`] first.
    pub fn new(db: &'a Db) -> Self {
        EmbeddingStore { db }
    }

    /// Create the `embeddings` table if it does not exist. Idempotent.
    pub fn init(&self) -> Result<()> {
        self.db.conn().execute_batch(EMBEDDINGS_SCHEMA)?;
        Ok(())
    }

    /// Insert or replace the embedding for `message_id`, tagging it with the
    /// producing `model`.
    pub fn put(&self, message_id: i64, model: &str, vec: &[f32]) -> Result<()> {
        let blob = encode_vec(vec);
        self.db.conn().execute(
            "INSERT OR REPLACE INTO embeddings (message_id, model, vec) VALUES (?1, ?2, ?3)",
            params![message_id, model, blob],
        )?;
        Ok(())
    }

    /// Load the embedding for `message_id`, or `None` if none is stored.
    pub fn get(&self, message_id: i64) -> Result<Option<Vec<f32>>> {
        let blob: Option<Vec<u8>> = self
            .db
            .conn()
            .query_row(
                "SELECT vec FROM embeddings WHERE message_id = ?1",
                params![message_id],
                |r| r.get(0),
            )
            .optional()?;
        match blob {
            Some(b) => Ok(Some(decode_vec(&b)?)),
            None => Ok(None),
        }
    }

    /// Load every stored `(message_id, vector)` for `model`, ordered by id.
    ///
    /// This is the candidate set a vector search ranks against. Restricting to
    /// a single model keeps dimensionalities consistent within one query.
    pub fn load_all(&self, model: &str) -> Result<Vec<(i64, Vec<f32>)>> {
        let mut stmt = self.db.conn().prepare(
            "SELECT message_id, vec FROM embeddings WHERE model = ?1 ORDER BY message_id",
        )?;
        let rows = stmt.query_map(params![model], |r| {
            let id: i64 = r.get(0)?;
            let blob: Vec<u8> = r.get(1)?;
            Ok((id, blob))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, blob) = row?;
            out.push((id, decode_vec(&blob)?));
        }
        Ok(out)
    }

    /// Number of stored embeddings for `model`.
    pub fn count(&self, model: &str) -> Result<i64> {
        let n: i64 = self.db.conn().query_row(
            "SELECT COUNT(*) FROM embeddings WHERE model = ?1",
            params![model],
            |r| r.get(0),
        )?;
        Ok(n)
    }
}

/// A deterministic in-memory [`Embedder`] for tests.
///
/// It produces a fixed-dimension vector from each input string via a simple
/// bag-of-bytes hash, so identical strings embed identically and similar
/// strings embed similarly — enough to exercise the store and fusion paths
/// without a network call. Not exported from the crate root; test-only.
#[derive(Debug, Clone)]
pub struct FakeEmbedder {
    /// Output dimensionality.
    pub dim: usize,
}

impl FakeEmbedder {
    /// Build a fake embedder producing `dim`-dimensional vectors.
    pub fn new(dim: usize) -> Self {
        FakeEmbedder { dim: dim.max(1) }
    }
}

impl Embedder for FakeEmbedder {
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let mut out = Vec::with_capacity(texts.len());
        for t in texts {
            let mut v = vec![0.0f32; self.dim];
            for (i, b) in t.bytes().enumerate() {
                v[i % self.dim] += (b as f32) / 255.0;
            }
            out.push(v);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_identical_is_one() {
        let a = vec![1.0, 2.0, 3.0];
        let s = cosine(&a, &a);
        assert!((s - 1.0).abs() < 1e-6, "identical vectors -> 1, got {s}");
    }

    #[test]
    fn cosine_orthogonal_is_zero() {
        let a = vec![1.0, 0.0];
        let b = vec![0.0, 1.0];
        assert!(cosine(&a, &b).abs() < 1e-6);
    }

    #[test]
    fn cosine_opposite_is_negative_one() {
        let a = vec![1.0, 1.0];
        let b = vec![-1.0, -1.0];
        assert!((cosine(&a, &b) + 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_length_mismatch_is_zero() {
        assert_eq!(cosine(&[1.0, 2.0], &[1.0]), 0.0);
    }

    #[test]
    fn cosine_zero_vector_is_zero() {
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 1.0]), 0.0);
        assert_eq!(cosine(&[1.0, 1.0], &[0.0, 0.0]), 0.0);
    }

    #[test]
    fn encode_decode_round_trip() {
        let v = vec![0.0, -1.5, 3.25, 1234.5];
        let bytes = encode_vec(&v);
        assert_eq!(bytes.len(), v.len() * 4);
        let back = decode_vec(&bytes).unwrap();
        assert_eq!(v, back);
    }

    #[test]
    fn decode_rejects_bad_length() {
        let err = decode_vec(&[0, 1, 2]).unwrap_err();
        assert!(err.to_string().contains("multiple of 4"));
    }

    #[test]
    fn decode_empty_is_empty() {
        assert_eq!(decode_vec(&[]).unwrap(), Vec::<f32>::new());
    }

    #[test]
    fn rrf_rewards_agreement() {
        // "b" is top of list_a and second of list_b; "a" is second of list_a
        // and top of list_b. Both appear high in both lists, so they should
        // outrank items that appear in only one list.
        let a = vec!["b", "a", "c"];
        let b = vec!["a", "b", "d"];
        let fused = rrf_default(&a, &b);
        let top2: Vec<&str> = fused.iter().take(2).map(|(k, _)| *k).collect();
        assert!(top2.contains(&"a"));
        assert!(top2.contains(&"b"));
        // "c" and "d" each appear in only one list -> lower.
        let last2: Vec<&str> = fused.iter().skip(2).map(|(k, _)| *k).collect();
        assert!(last2.contains(&"c"));
        assert!(last2.contains(&"d"));
    }

    #[test]
    fn rrf_position_matters() {
        // Same single-list membership; earlier position -> higher fused score.
        let a = vec![10i64, 20, 30];
        let empty: Vec<i64> = Vec::new();
        let fused = rrf_default(&a, &empty);
        assert_eq!(fused[0].0, 10);
        assert_eq!(fused[1].0, 20);
        assert_eq!(fused[2].0, 30);
        assert!(fused[0].1 > fused[1].1);
        assert!(fused[1].1 > fused[2].1);
    }

    #[test]
    fn rrf_k_changes_contribution() {
        let a = vec![1i64];
        let empty: Vec<i64> = Vec::new();
        let small_k = rrf(&a, &empty, 1.0)[0].1;
        let big_k = rrf(&a, &empty, 100.0)[0].1;
        // 1/(1+0+1)=0.5 vs 1/(100+0+1)~=0.0099
        assert!((small_k - 0.5).abs() < 1e-9);
        assert!(big_k < small_k);
    }

    #[test]
    fn rrf_tie_breaks_by_key() {
        // Two keys with identical fused score (each appears once at rank 0 in a
        // separate list) sort by key order.
        let a = vec![5i64];
        let b = vec![2i64];
        let fused = rrf_default(&a, &b);
        assert_eq!(fused[0].1, fused[1].1);
        assert_eq!(fused[0].0, 2, "lower key first on tie");
        assert_eq!(fused[1].0, 5);
    }

    #[test]
    fn rank_by_cosine_orders_by_similarity() {
        let query = vec![1.0f32, 0.0];
        let cands = vec![
            (1i64, vec![0.0f32, 1.0]),  // orthogonal
            (2i64, vec![1.0f32, 0.0]),  // identical
            (3i64, vec![-1.0f32, 0.0]), // opposite
        ];
        let ranked = rank_by_cosine(&query, &cands);
        assert_eq!(ranked, vec![2, 1, 3]);
    }

    #[test]
    fn rank_by_cosine_tie_breaks_by_id() {
        let query = vec![1.0f32, 0.0];
        let cands = vec![(9i64, vec![1.0f32, 0.0]), (4i64, vec![1.0f32, 0.0])];
        let ranked = rank_by_cosine(&query, &cands);
        assert_eq!(ranked, vec![4, 9]);
    }

    #[test]
    fn store_round_trip_and_missing() {
        let db = Db::open(":memory:").unwrap();
        let store = EmbeddingStore::new(&db);
        store.init().unwrap();
        store.init().unwrap(); // idempotent

        assert_eq!(store.get(1).unwrap(), None);
        assert_eq!(store.count("m").unwrap(), 0);

        let v = vec![0.1f32, 0.2, 0.3];
        store.put(1, "m", &v).unwrap();
        assert_eq!(store.get(1).unwrap(), Some(v.clone()));
        assert_eq!(store.count("m").unwrap(), 1);

        // Replace overwrites in place.
        let v2 = vec![9.0f32, 8.0, 7.0];
        store.put(1, "m", &v2).unwrap();
        assert_eq!(store.get(1).unwrap(), Some(v2));
        assert_eq!(store.count("m").unwrap(), 1);
    }

    #[test]
    fn store_load_all_filters_by_model_and_orders() {
        let db = Db::open(":memory:").unwrap();
        let store = EmbeddingStore::new(&db);
        store.init().unwrap();
        store.put(3, "m1", &[1.0, 0.0]).unwrap();
        store.put(1, "m1", &[0.0, 1.0]).unwrap();
        store.put(2, "m2", &[1.0, 1.0]).unwrap();

        let all = store.load_all("m1").unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].0, 1, "ordered by message_id");
        assert_eq!(all[1].0, 3);
        assert_eq!(store.load_all("m2").unwrap().len(), 1);
        assert_eq!(store.load_all("nope").unwrap().len(), 0);
    }

    #[test]
    fn fake_embedder_is_deterministic_and_shaped() {
        let e = FakeEmbedder::new(4);
        let out = e
            .embed(&[
                "hello".to_string(),
                "hello".to_string(),
                "world".to_string(),
            ])
            .unwrap();
        assert_eq!(out.len(), 3);
        assert!(out.iter().all(|v| v.len() == 4));
        assert_eq!(out[0], out[1], "identical text -> identical vector");
        assert_ne!(out[0], out[2]);
    }

    #[test]
    fn fake_embedder_min_dim_one() {
        let e = FakeEmbedder::new(0);
        assert_eq!(e.dim, 1);
        let out = e.embed(&["x".to_string()]).unwrap();
        assert_eq!(out[0].len(), 1);
    }

    #[test]
    fn store_methods_report_sql_errors() {
        // A store whose table was never created drives every SQL call down its
        // `?` error arm.
        let db = Db::open(":memory:").unwrap();
        let store = EmbeddingStore::new(&db);
        assert!(store.put(1, "m", &[0.1, 0.2]).is_err());
        assert!(store.get(1).is_err());
        assert!(store.load_all("m").is_err());
        assert!(store.count("m").is_err());
    }
}
