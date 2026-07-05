//! Portable file identity.
//!
//! A transcript's `file_key` must stay stable even if the file is renamed or
//! moved (transcripts get relocated between session directories), so on unix we
//! key off the inode/device pair rather than the path. The only non-portable
//! call — reading `dev`/`ino` off [`std::fs::Metadata`] — is isolated in
//! [`identity_from_metadata`]; the rest is a plain, testable string mapping.

use std::path::Path;

use crate::error::Result;

/// Compute a stable, source-scoped identity for a transcript file.
///
/// On unix this is `"<dev>:<ino>"` derived from the file's metadata. If the
/// metadata cannot be read (missing file, unsupported platform), it falls back
/// to the canonicalized path, and finally to the path as given.
pub fn file_key(path: &Path) -> String {
    if let Ok(meta) = std::fs::metadata(path) {
        if let Some(id) = identity_from_metadata(&meta) {
            return id;
        }
    }
    canonical_key(path)
}

/// Fall back to a path-based key: the canonical path if resolvable, else the
/// lossy display form of the path as provided.
fn canonical_key(path: &Path) -> String {
    match std::fs::canonicalize(path) {
        Ok(p) => p.to_string_lossy().into_owned(),
        Err(_) => path.to_string_lossy().into_owned(),
    }
}

/// Extract a `dev:ino` identity from unix metadata.
///
/// Kept tiny and unix-gated so the platform-specific `MetadataExt` surface is
/// the only thing behind a `cfg`; the caller and fallback logic stay portable
/// and testable. Returns `None` on non-unix targets.
#[cfg(unix)]
fn identity_from_metadata(meta: &std::fs::Metadata) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    Some(format!("{}:{}", meta.dev(), meta.ino()))
}

/// Non-unix fallback: no stable inode identity available.
#[cfg(not(unix))]
fn identity_from_metadata(_meta: &std::fs::Metadata) -> Option<String> {
    None
}

/// Read `(size, mtime_secs)` for a file, best-effort.
///
/// `mtime` is unix seconds; a metadata failure yields `(0, 0)` so discovery can
/// still record the file. This mirrors the fields on [`crate::source::SessionFile`].
pub fn size_and_mtime(path: &Path) -> Result<(u64, i64)> {
    let meta = std::fs::metadata(path)?;
    let size = meta.len();
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    Ok((size, mtime))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tempdir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("csx-fileid-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn key_is_stable_across_calls() {
        let dir = tempdir("stable");
        let p = dir.join("a.jsonl");
        std::fs::File::create(&p).unwrap().write_all(b"x").unwrap();
        let k1 = file_key(&p);
        let k2 = file_key(&p);
        assert_eq!(k1, k2);
        assert!(!k1.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn key_survives_rename_on_unix() {
        let dir = tempdir("rename");
        let p1 = dir.join("before.jsonl");
        let p2 = dir.join("after.jsonl");
        std::fs::File::create(&p1).unwrap().write_all(b"x").unwrap();
        let before = file_key(&p1);
        std::fs::rename(&p1, &p2).unwrap();
        let after = file_key(&p2);
        // Same inode -> same key despite the different path.
        assert_eq!(before, after);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn distinct_files_have_distinct_keys() {
        let dir = tempdir("distinct");
        let a = dir.join("a.jsonl");
        let b = dir.join("b.jsonl");
        std::fs::File::create(&a).unwrap();
        std::fs::File::create(&b).unwrap();
        assert_ne!(file_key(&a), file_key(&b));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_file_falls_back_to_path() {
        let dir = tempdir("missing");
        let p = dir.join("nope.jsonl");
        // No file created: metadata fails, so we get the path form back.
        let k = file_key(&p);
        assert!(k.contains("nope.jsonl"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn size_and_mtime_reads_metadata() {
        let dir = tempdir("meta");
        let p = dir.join("m.jsonl");
        std::fs::File::create(&p)
            .unwrap()
            .write_all(b"hello")
            .unwrap();
        let (size, mtime) = size_and_mtime(&p).unwrap();
        assert_eq!(size, 5);
        assert!(mtime > 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn size_and_mtime_missing_is_error() {
        let dir = tempdir("meta-missing");
        let p = dir.join("gone.jsonl");
        assert!(size_and_mtime(&p).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn canonical_key_resolves_existing_and_missing() {
        let dir = tempdir("canon");
        let p = dir.join("here.jsonl");
        std::fs::File::create(&p).unwrap().write_all(b"y").unwrap();
        // An existing path canonicalizes (the `Ok` arm), yielding an absolute,
        // non-empty key that names the file.
        let k = canonical_key(&p);
        assert!(k.contains("here.jsonl"));
        // A missing path falls back to the lossy display form (the `Err` arm).
        let gone = dir.join("gone.jsonl");
        assert_eq!(canonical_key(&gone), gone.to_string_lossy().into_owned());
        std::fs::remove_dir_all(&dir).ok();
    }
}
