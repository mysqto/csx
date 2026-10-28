//! Real detached-process adapter (EXCLUDED from coverage).
//!
//! [`RealSpawner`] starts a child process in its own process group with all
//! stdio closed and does not wait for it. Whether the child runs cannot be asserted in a unit test, so
//! it lives behind the [`Spawner`] port. All decision logic is in
//! [`crate::refresh`].

use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::error::Result;
use crate::refresh::Spawner;

/// [`Spawner`] backed by [`std::process::Command`].
pub struct RealSpawner;

impl Spawner for RealSpawner {
    fn spawn_detached(&self, program: &Path, args: &[&str]) -> Result<()> {
        // The child handle is dropped on purpose: csx does not wait for it.
        // Its own process group keeps it alive when the caller's group is
        // killed, as Alfred and Raycast do to a script they stop, so the
        // sync finishes and releases its lock.
        Command::new(program)
            .args(args)
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        Ok(())
    }
}
