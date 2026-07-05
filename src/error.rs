//! Canonical error type for the crate.

use thiserror::Error;

/// Errors produced across the csx crate.
#[derive(Debug, Error)]
pub enum Error {
    /// An I/O operation failed.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// A JSON (de)serialization operation failed.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    /// A SQLite operation failed.
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    /// A source transcript could not be parsed.
    #[error("parse error: {0}")]
    Parse(String),

    /// An unknown enum value was encountered.
    #[error("unknown value: {0}")]
    Unknown(String),

    /// A generic, contextual error.
    #[error("{0}")]
    Other(String),
}

/// Convenience result alias used throughout the crate.
pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    /// Build a parse error from anything string-like.
    pub fn parse(msg: impl Into<String>) -> Self {
        Error::Parse(msg.into())
    }

    /// Build a generic error from anything string-like.
    pub fn other(msg: impl Into<String>) -> Self {
        Error::Other(msg.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constructors_and_display() {
        let p = Error::parse("bad line");
        assert_eq!(p.to_string(), "parse error: bad line");
        let o = Error::other("boom");
        assert_eq!(o.to_string(), "boom");
        let u = Error::Unknown("x".into());
        assert_eq!(u.to_string(), "unknown value: x");
    }

    #[test]
    fn from_conversions() {
        let io: Error = std::io::Error::new(std::io::ErrorKind::NotFound, "nope").into();
        assert!(matches!(io, Error::Io(_)));
        assert!(io.to_string().contains("io error"));

        let json_err = serde_json::from_str::<i32>("not json").unwrap_err();
        let e: Error = json_err.into();
        assert!(matches!(e, Error::Json(_)));
        assert!(e.to_string().starts_with("json error:"));

        let sql: Error = rusqlite::Error::QueryReturnedNoRows.into();
        assert!(matches!(sql, Error::Sqlite(_)));
        assert!(sql.to_string().starts_with("sqlite error:"));
    }
}
