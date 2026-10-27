//! Real adapters for the tab ports: osascript, subprocesses, tty devices and
//! sleeping. No logic lives here.

use super::{exit_result, CommandRunner, OsaRunner, Sleeper, TtyWriter};
use crate::error::{Error, Result};
use std::io::Write;
use std::process::{Command, Output};

fn finish(what: &str, out: Output) -> Result<String> {
    exit_result(what, out.status.success(), &out.stdout, &out.stderr)
}

/// Runs JXA through `/usr/bin/osascript`.
pub struct RealOsa;

impl OsaRunner for RealOsa {
    fn run_jxa(&self, script: &str) -> Result<String> {
        let out = Command::new("/usr/bin/osascript")
            .args(["-l", "JavaScript", "-e", script])
            .output()?;
        finish("osascript", out)
    }
}

/// Runs commands with `std::process::Command`.
pub struct RealCommand;

impl CommandRunner for RealCommand {
    fn run(&self, program: &str, args: &[&str]) -> Result<String> {
        let out = Command::new(program)
            .args(args)
            .output()
            .map_err(|e| Error::other(format!("{program}: {e}")))?;
        finish(program, out)
    }
}

/// Writes to `/dev/<tty>`.
pub struct RealTty;

impl TtyWriter for RealTty {
    fn write(&self, tty: &str, bytes: &[u8]) -> Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .open(format!("/dev/{tty}"))?;
        f.write_all(bytes)?;
        Ok(())
    }
}

/// Sleeps the current thread.
pub struct RealSleeper;

impl Sleeper for RealSleeper {
    fn sleep_ms(&self, ms: u64) {
        std::thread::sleep(std::time::Duration::from_millis(ms));
    }
}
