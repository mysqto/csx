//! Shared JSONL tailing helper.
//!
//! Transcripts are append-only newline-delimited JSON. Both source adapters
//! resume parsing from a persisted byte offset (the watermark), so this module
//! provides one primitive: read every *complete* line appended at or after a
//! byte offset, returning the parsed values together with the new offset to
//! persist.
//!
//! Reading a file under a temp root is fully testable, so nothing here needs a
//! shim; only a live filesystem *watcher* would.

use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use serde_json::Value;

use crate::error::Result;

/// The outcome of tailing a JSONL file from a byte offset.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct JsonlChunk {
    /// The parsed JSON value of each complete line read, in file order.
    pub values: Vec<Value>,
    /// The byte offset just past the last complete line consumed. Persist this
    /// and pass it back as `offset` on the next call to resume without
    /// re-reading.
    pub new_offset: u64,
}

/// Read complete JSONL lines from `path` starting at byte `offset`.
///
/// A "complete" line is one terminated by `\n`. Any trailing bytes without a
/// terminating newline (a partial line still being written) are left
/// unconsumed: they are not parsed and `new_offset` stops before them, so the
/// next call re-reads and completes them.
///
/// Blank lines are skipped. A line that fails to parse as JSON is a hard error
/// — callers decide how tolerant to be, but by construction a fully-written
/// transcript line is valid JSON.
///
/// When `offset` is beyond the current end of file (e.g. the file was
/// truncated/rotated), no bytes are read and `new_offset == offset`.
pub fn read_from_offset(path: &Path, offset: u64) -> Result<JsonlChunk> {
    let file = std::fs::File::open(path)?;
    let total = file.metadata()?.len();
    if offset >= total {
        return Ok(JsonlChunk {
            values: Vec::new(),
            new_offset: offset,
        });
    }

    let mut reader = BufReader::new(file);
    reader.seek(SeekFrom::Start(offset))?;

    let mut values = Vec::new();
    let mut consumed = offset;
    let mut line = String::new();
    loop {
        line.clear();
        let n = read_line(&mut reader, &mut line)?;
        if n == 0 {
            break; // EOF
        }
        if !line.ends_with('\n') {
            // Partial final line: leave it unconsumed for the next call.
            break;
        }
        consumed += n as u64;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(trimmed)?;
        values.push(value);
    }

    Ok(JsonlChunk {
        values,
        new_offset: consumed,
    })
}

/// Read a single line (including any trailing `\n`) as UTF-8, returning the
/// number of raw bytes consumed. Wraps [`BufRead::read_line`] but treats
/// invalid UTF-8 as a parse error rather than lossily.
fn read_line<R: BufRead>(reader: &mut R, out: &mut String) -> Result<usize> {
    // `read_line` handles UTF-8 validation and errors on invalid sequences.
    let n = reader.read_line(out)?;
    Ok(n)
}

/// Read an entire small JSON document (not JSONL) from `path`, if it exists.
///
/// Returns `Ok(None)` when the file is absent, so callers can treat a missing
/// account file as "no account" without special-casing [`std::io::ErrorKind`].
pub fn read_json_file(path: &Path) -> Result<Option<Value>> {
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let mut buf = String::new();
    file.read_to_string(&mut buf)?;
    if buf.trim().is_empty() {
        return Ok(None);
    }
    let value: Value = serde_json::from_str(&buf)?;
    Ok(Some(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tempfile(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "csx-jsonl-{}-{}",
            std::process::id(),
            // A per-name subdir keeps parallel tests from colliding.
            name
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(format!("{name}.jsonl"))
    }

    fn write(path: &Path, contents: &str) {
        let mut f = std::fs::File::create(path).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
    }

    fn append(path: &Path, contents: &str) {
        let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
    }

    #[test]
    fn reads_all_lines_from_start() {
        let p = tempfile("all");
        write(&p, "{\"a\":1}\n{\"b\":2}\n");
        let chunk = read_from_offset(&p, 0).unwrap();
        assert_eq!(chunk.values.len(), 2);
        assert_eq!(chunk.values[0]["a"], 1);
        assert_eq!(chunk.values[1]["b"], 2);
        assert_eq!(chunk.new_offset, p.metadata().unwrap().len());
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn skips_blank_lines() {
        let p = tempfile("blank");
        write(&p, "{\"a\":1}\n\n   \n{\"b\":2}\n");
        let chunk = read_from_offset(&p, 0).unwrap();
        assert_eq!(chunk.values.len(), 2);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn resumes_from_nonzero_offset() {
        let p = tempfile("resume");
        write(&p, "{\"a\":1}\n");
        let first = read_from_offset(&p, 0).unwrap();
        assert_eq!(first.values.len(), 1);
        // Append more content and resume from the recorded offset.
        append(&p, "{\"b\":2}\n{\"c\":3}\n");
        let second = read_from_offset(&p, first.new_offset).unwrap();
        assert_eq!(second.values.len(), 2);
        assert_eq!(second.values[0]["b"], 2);
        assert_eq!(second.values[1]["c"], 3);
        assert_eq!(second.new_offset, p.metadata().unwrap().len());
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn leaves_partial_final_line_unconsumed() {
        let p = tempfile("partial");
        // Second line has no terminating newline yet.
        write(&p, "{\"a\":1}\n{\"b\":2}");
        let chunk = read_from_offset(&p, 0).unwrap();
        assert_eq!(chunk.values.len(), 1);
        assert_eq!(chunk.new_offset, 8); // only the first line consumed
                                         // Completing the line makes it readable on the next call.
        append(&p, "\n");
        let chunk2 = read_from_offset(&p, chunk.new_offset).unwrap();
        assert_eq!(chunk2.values.len(), 1);
        assert_eq!(chunk2.values[0]["b"], 2);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn offset_past_eof_reads_nothing() {
        let p = tempfile("past");
        write(&p, "{\"a\":1}\n");
        let len = p.metadata().unwrap().len();
        let chunk = read_from_offset(&p, len).unwrap();
        assert!(chunk.values.is_empty());
        assert_eq!(chunk.new_offset, len);
        // Well past EOF is also safe.
        let chunk = read_from_offset(&p, len + 1000).unwrap();
        assert!(chunk.values.is_empty());
        assert_eq!(chunk.new_offset, len + 1000);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn invalid_json_line_is_an_error() {
        let p = tempfile("bad");
        write(&p, "not json\n");
        assert!(read_from_offset(&p, 0).is_err());
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn read_json_file_missing_is_none() {
        let p = tempfile("missing-account");
        std::fs::remove_file(&p).ok();
        assert_eq!(read_json_file(&p).unwrap(), None);
    }

    #[test]
    fn read_json_file_empty_is_none() {
        let p = tempfile("empty-account");
        write(&p, "   \n");
        assert_eq!(read_json_file(&p).unwrap(), None);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn read_json_file_parses() {
        let p = tempfile("account");
        write(&p, "{\"x\": 42}");
        let v = read_json_file(&p).unwrap().unwrap();
        assert_eq!(v["x"], 42);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn read_json_file_missing_is_ok_none() {
        let p = tempfile("absent");
        std::fs::remove_file(&p).ok();
        assert_eq!(read_json_file(&p).unwrap(), None);
    }

    #[test]
    fn read_json_file_empty_is_ok_none() {
        let p = tempfile("json-blank");
        write(&p, "   \n  ");
        assert_eq!(read_json_file(&p).unwrap(), None);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn read_json_file_non_notfound_error_propagates() {
        // Use a regular file as if it were a directory: opening `<file>/child`
        // fails with a not-a-directory error (not NotFound), taking the generic
        // error arm.
        let f = tempfile("notdir");
        write(&f, "{}");
        let child = f.join("child.json");
        assert!(read_json_file(&child).is_err());
        std::fs::remove_file(&f).ok();
    }
}
