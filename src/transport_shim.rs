//! Request-transport port and its real Unix-socket adapter.
//!
//! The daemon speaks a tiny line protocol: a client connects, writes one
//! request line, reads one response line, and disconnects. That shape is
//! captured by two ports — [`Listener`] (accepts connections) and [`Conn`]
//! (reads a request line, writes a response) — so the whole request/response
//! loop in [`crate::daemon`] can be driven by in-memory fakes.
//!
//! The only real implementation, [`UnixSocketListener`], binds a `std`
//! [`UnixListener`]; it and its [`UnixConn`] touch the OS and so live here,
//! excluded from coverage.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// Port that yields inbound connections one at a time.
pub trait Listener {
    /// The connection type produced by [`Listener::accept`].
    type Conn: Conn;

    /// Block until a client connects, returning the connection.
    fn accept(&self) -> Result<Self::Conn>;
}

/// Port over a single accepted connection: read exactly one request line, then
/// write exactly one response.
pub trait Conn {
    /// Read a single newline-terminated request line (without the newline).
    ///
    /// `Ok(None)` means the peer closed without sending anything.
    fn read_line(&mut self) -> Result<Option<String>>;

    /// Write `bytes` back to the peer.
    fn write_all(&mut self, bytes: &[u8]) -> Result<()>;
}

/// [`Listener`] backed by a bound Unix domain socket.
pub struct UnixSocketListener {
    inner: UnixListener,
    path: PathBuf,
}

impl UnixSocketListener {
    /// Bind a Unix socket at `path`, removing any stale socket file first.
    pub fn bind(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        let inner = UnixListener::bind(&path)
            .map_err(|e| Error::other(format!("bind {} failed: {e}", path.display())))?;
        Ok(UnixSocketListener { inner, path })
    }
}

impl Drop for UnixSocketListener {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

impl Listener for UnixSocketListener {
    type Conn = UnixConn;

    fn accept(&self) -> Result<Self::Conn> {
        let (stream, _addr) = self.inner.accept()?;
        Ok(UnixConn::new(stream))
    }
}

/// A single accepted [`UnixStream`] connection.
pub struct UnixConn {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl UnixConn {
    fn new(stream: UnixStream) -> Self {
        let writer = stream.try_clone().expect("clone unix stream");
        UnixConn {
            reader: BufReader::new(stream),
            writer,
        }
    }
}

impl Conn for UnixConn {
    fn read_line(&mut self) -> Result<Option<String>> {
        let mut line = String::new();
        let n = self.reader.read_line(&mut line)?;
        if n == 0 {
            return Ok(None);
        }
        Ok(Some(line.trim_end_matches(['\r', '\n']).to_string()))
    }

    fn write_all(&mut self, bytes: &[u8]) -> Result<()> {
        self.writer.write_all(bytes)?;
        self.writer.flush()?;
        Ok(())
    }
}
