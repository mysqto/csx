//! Real git invocation adapter.
//!
//! This is the ONLY place the crate shells out to the `git` binary. All
//! decision logic that consumes git output lives in [`crate::repo`] behind the
//! [`GitRunner`] trait, so this file is excluded from coverage.

use std::process::Command;

use crate::error::{Error, Result};

/// A port that runs a git subcommand in a working directory and returns its
/// stdout on success.
///
/// Implementations must not panic; a non-zero exit (or a missing binary) is an
/// [`Err`]. The real implementation lives here; tests use fakes.
pub trait GitRunner {
    /// Run `git <args...>` with `cwd` as the working directory, returning the
    /// captured stdout on a zero exit status.
    fn run(&self, cwd: &str, args: &[&str]) -> Result<String>;
}

/// [`GitRunner`] implementation that spawns the system `git` binary.
#[derive(Debug, Default, Clone, Copy)]
pub struct ProcessGit;

impl GitRunner for ProcessGit {
    fn run(&self, cwd: &str, args: &[&str]) -> Result<String> {
        let output = Command::new("git").current_dir(cwd).args(args).output()?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(Error::other(format!(
                "git {} failed: {}",
                args.join(" "),
                stderr.trim()
            )));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}
