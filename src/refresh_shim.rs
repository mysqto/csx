//! Real detached-process adapter (EXCLUDED from coverage).
//!
//! [`RealSpawner`] starts a child process with all stdio closed and does not
//! wait for it. Whether the child runs cannot be asserted in a unit test, so
//! it lives behind the [`Spawner`] port. All decision logic is in
//! [`crate::refresh`].

use std::path::Path;
use std::process::{Command, Stdio};

use crate::error::Result;
use crate::refresh::Spawner;

/// [`Spawner`] backed by [`std::process::Command`].
pub struct RealSpawner;

impl Spawner for RealSpawner {
    fn spawn_detached(&self, program: &Path, args: &[&str]) -> Result<()> {
        // The child handle is dropped on purpose: csx does not wait for it.
        Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        Ok(())
    }
}
