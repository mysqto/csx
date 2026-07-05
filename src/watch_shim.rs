//! Filesystem-watching port and its real adapter.
//!
//! The [`FileWatcher`] trait is the port the daemon consumes: it yields
//! [`WatchEvent`]s describing paths that changed under a set of watched roots.
//! The only real, OS-touching implementation is [`NotifyWatcher`], which drives
//! the `notify` crate on a background thread and forwards events over a channel.
//! Because live filesystem watching cannot be exercised deterministically in a
//! unit test, this file is excluded from coverage; all daemon logic consumes the
//! trait and is driven by a fake in tests.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use notify::{Event, RecursiveMode, Watcher};

use crate::error::{Error, Result};

/// A single filesystem change observed under a watched root.
///
/// The daemon does not care *what* changed, only that a path did, so a
/// [`WatchEvent`] carries just the affected path; the debouncer coalesces
/// bursts and the indexer re-scans affected sources.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchEvent {
    /// The path that changed (created, modified, or removed).
    pub path: PathBuf,
}

impl WatchEvent {
    /// Build a [`WatchEvent`] for `path`.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        WatchEvent { path: path.into() }
    }
}

/// Port over a source of filesystem change events.
///
/// [`FileWatcher::poll`] returns the next event, blocking up to `timeout` for
/// one to arrive; `Ok(None)` means the timeout elapsed with no event (which the
/// daemon uses to drive its debounce clock), while an [`Err`] means the watch
/// backend has terminated and the daemon should stop.
pub trait FileWatcher {
    /// Wait up to `timeout` for the next change event.
    fn poll(&mut self, timeout: Duration) -> Result<Option<WatchEvent>>;
}

/// [`FileWatcher`] backed by the `notify` crate.
///
/// Watches every root recursively; each raw `notify` event is flattened into
/// one [`WatchEvent`] per affected path and pushed onto an internal channel.
pub struct NotifyWatcher {
    _watcher: notify::RecommendedWatcher,
    rx: Receiver<WatchEvent>,
}

impl NotifyWatcher {
    /// Begin watching every path in `roots` recursively.
    pub fn new(roots: &[PathBuf]) -> Result<Self> {
        let (tx, rx) = mpsc::channel::<WatchEvent>();
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
            if let Ok(event) = res {
                for path in event.paths {
                    let _ = tx.send(WatchEvent::new(path));
                }
            }
        })
        .map_err(|e| Error::other(format!("watcher init failed: {e}")))?;

        for root in roots {
            watch_one(&mut watcher, root)?;
        }

        Ok(NotifyWatcher {
            _watcher: watcher,
            rx,
        })
    }
}

/// Watch a single root, ignoring a not-found root (a tool may be absent).
fn watch_one(watcher: &mut notify::RecommendedWatcher, root: &Path) -> Result<()> {
    match watcher.watch(root, RecursiveMode::Recursive) {
        Ok(()) => Ok(()),
        Err(e) if is_missing(&e) => Ok(()),
        Err(e) => Err(Error::other(format!(
            "watch {} failed: {e}",
            root.display()
        ))),
    }
}

/// Whether a `notify` error is a benign "path does not exist" condition.
fn is_missing(e: &notify::Error) -> bool {
    matches!(
        &e.kind,
        notify::ErrorKind::Io(io) if io.kind() == std::io::ErrorKind::NotFound
    )
}

impl FileWatcher for NotifyWatcher {
    fn poll(&mut self, timeout: Duration) -> Result<Option<WatchEvent>> {
        match self.rx.recv_timeout(timeout) {
            Ok(ev) => Ok(Some(ev)),
            Err(mpsc::RecvTimeoutError::Timeout) => Ok(None),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err(Error::other("watch backend disconnected"))
            }
        }
    }
}
