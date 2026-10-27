//! Parsing of `ps` output into a process table.

use super::CommandRunner;
use crate::error::Result;

/// Arguments for `ps` that produce the columns [`ProcessTable::parse`] reads.
pub const PS_ARGS: &[&str] = &["-axo", "pid=,ppid=,pgid=,tpgid=,tty=,etime=,args="];

/// Interpreters whose first script argument names the real program.
const INTERPRETERS: &[&str] = &["node", "bun", "deno", "python", "python3", "ruby"];
/// Script names that say nothing about the program.
const GENERIC_SCRIPTS: &[&str] = &["index.js", "cli.js", "main.js"];
/// Folder names skipped when looking for a program name in a script path.
const SKIPPED_DIRS: &[&str] = &["dist", "bin", "lib", "build"];

/// One row of `ps` output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Process {
    /// Process id.
    pub pid: u32,
    /// Parent process id.
    pub ppid: u32,
    /// Process group id.
    pub pgid: u32,
    /// Foreground process group id of the controlling tty (0 when none).
    pub tpgid: u32,
    /// Controlling tty without `/dev/`, or `None`.
    pub tty: Option<String>,
    /// Elapsed run time in seconds.
    pub elapsed_secs: u64,
    /// Full command line.
    pub args: String,
}

fn basename(path: &str) -> &str {
    path.rsplit_once('/').map_or(path, |(_, b)| b)
}

/// True for `node`, `python3.12` and the other interpreters, by basename.
fn is_interpreter(name: &str) -> bool {
    INTERPRETERS.iter().any(|i| {
        name.strip_prefix(i)
            .is_some_and(|rest| rest.chars().all(|c| c.is_ascii_digit() || c == '.'))
    })
}

/// Program name for a script path: its basename, or for generic names the
/// nearest parent folder that is not a build folder.
fn script_name(path: &str) -> String {
    let base = basename(path);
    if !GENERIC_SCRIPTS.contains(&base) {
        return base.to_string();
    }
    let mut parts = path.rsplit('/').skip(1);
    parts
        .find(|p| !p.is_empty() && !SKIPPED_DIRS.contains(p))
        .unwrap_or(base)
        .to_string()
}

impl Process {
    /// Basename of argv[0]. For interpreters (node, bun, deno, python, python3,
    /// ruby, also versioned) it is the module after `-m`, or else the program
    /// name of the first non-flag argument.
    pub fn program(&self) -> String {
        let mut toks = self.args.split_whitespace();
        let first = basename(toks.next().unwrap_or(""));
        if !is_interpreter(first) {
            return first.to_string();
        }
        while let Some(t) = toks.next() {
            if t == "-m" {
                return toks.next().unwrap_or(first).to_string();
            }
            if !t.starts_with('-') {
                return script_name(t);
            }
        }
        first.to_string()
    }

    /// True when the process is in the foreground group of its tty.
    pub fn is_foreground(&self) -> bool {
        self.pgid == self.tpgid
    }
}

/// Parse `[[dd-]hh:]mm:ss` into seconds.
pub fn parse_etime(s: &str) -> Option<u64> {
    let (days, rest) = match s.split_once('-') {
        Some((d, r)) => (d.parse::<u64>().ok()?, r),
        None => (0, s),
    };
    let nums = rest
        .split(':')
        .map(|p| p.parse::<u64>().ok())
        .collect::<Option<Vec<_>>>()?;
    let secs = match nums.as_slice() {
        [m, s] => m * 60 + s,
        [h, m, s] => h * 3600 + m * 60 + s,
        _ => return None,
    };
    Some(days * 86400 + secs)
}

/// Take the next whitespace-delimited field, returning it and the remainder.
fn next_field(s: &str) -> Option<(&str, &str)> {
    let s = s.trim_start();
    if s.is_empty() {
        return None;
    }
    Some(s.split_once(char::is_whitespace).unwrap_or((s, "")))
}

fn parse_line(line: &str) -> Option<Process> {
    let (pid, rest) = next_field(line)?;
    let (ppid, rest) = next_field(rest)?;
    let (pgid, rest) = next_field(rest)?;
    let (tpgid, rest) = next_field(rest)?;
    let (tty, rest) = next_field(rest)?;
    let (etime, rest) = next_field(rest)?;
    let args = rest.trim();
    if args.is_empty() {
        return None;
    }
    Some(Process {
        pid: pid.parse().ok()?,
        ppid: ppid.parse().ok()?,
        pgid: pgid.parse().ok()?,
        tpgid: tpgid.parse::<i64>().ok()?.max(0) as u32,
        tty: (tty != "??").then(|| tty.to_string()),
        elapsed_secs: parse_etime(etime)?,
        args: args.to_string(),
    })
}

/// A snapshot of running processes.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProcessTable {
    /// All parsed processes, in `ps` order.
    pub procs: Vec<Process>,
}

impl ProcessTable {
    /// Parse `ps` output in the [`PS_ARGS`] layout. Malformed lines are skipped.
    pub fn parse(ps_output: &str) -> ProcessTable {
        ProcessTable {
            procs: ps_output.lines().filter_map(parse_line).collect(),
        }
    }

    /// Run `ps` through `cmd` and parse the result.
    pub fn load(cmd: &dyn CommandRunner) -> Result<ProcessTable> {
        Ok(ProcessTable::parse(&cmd.run("ps", PS_ARGS)?))
    }

    /// Look up a process by pid.
    pub fn get(&self, pid: u32) -> Option<&Process> {
        self.procs.iter().find(|p| p.pid == pid)
    }

    /// All processes whose controlling tty is `tty`.
    pub fn on_tty(&self, tty: &str) -> Vec<&Process> {
        self.procs
            .iter()
            .filter(|p| p.tty.as_deref() == Some(tty))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn proc(args: &str) -> Process {
        Process {
            pid: 1,
            ppid: 0,
            pgid: 1,
            tpgid: 1,
            tty: None,
            elapsed_secs: 0,
            args: args.into(),
        }
    }

    #[test]
    fn parse_reads_fields_and_args_with_spaces() {
        let t = ProcessTable::parse(
            "  501   1  501  501 ttys012    01:02:03 /Users/x/.local/bin/claude --resume abc\n",
        );
        assert_eq!(t.procs.len(), 1);
        let p = &t.procs[0];
        assert_eq!((p.pid, p.ppid, p.pgid, p.tpgid), (501, 1, 501, 501));
        assert_eq!(p.tty.as_deref(), Some("ttys012"));
        assert_eq!(p.elapsed_secs, 3723);
        assert_eq!(p.args, "/Users/x/.local/bin/claude --resume abc");
    }

    #[test]
    fn parse_maps_question_marks_to_no_tty() {
        let t = ProcessTable::parse("10 1 10 0 ?? 00:05 /sbin/launchd\n");
        assert_eq!(t.procs[0].tty, None);
        assert_eq!(t.procs[0].tpgid, 0);
    }

    #[test]
    fn parse_clamps_negative_tpgid() {
        let t = ProcessTable::parse("10 1 10 -1 ?? 00:05 /sbin/x\n");
        assert_eq!(t.procs[0].tpgid, 0);
    }

    #[test]
    fn parse_skips_malformed_lines() {
        let out = "\n\
                   garbage\n\
                   1 2 3 4 ttys001\n\
                   x 1 1 1 ttys001 00:01 a\n\
                   1 1 1 1 ttys001 bad a\n\
                   7 1 7 7 ttys001 00:01 ok\n";
        let t = ProcessTable::parse(out);
        assert_eq!(t.procs.len(), 1);
        assert_eq!(t.procs[0].pid, 7);
    }

    #[test]
    fn parse_etime_forms() {
        assert_eq!(parse_etime("05"), None);
        assert_eq!(parse_etime("01:02"), Some(62));
        assert_eq!(parse_etime("01:02:03"), Some(3723));
        assert_eq!(parse_etime("1-00:00:01"), Some(86401));
        assert_eq!(parse_etime("x-00:01"), None);
        assert_eq!(parse_etime("a:01"), None);
        assert_eq!(parse_etime("1:2:3:4"), None);
        assert_eq!(parse_etime(""), None);
    }

    #[test]
    fn program_uses_script_for_interpreters() {
        assert_eq!(
            proc("node /opt/lib/gemini/dist/index.js").program(),
            "gemini"
        );
        assert_eq!(proc("node /usr/local/bin/gemini chat").program(), "gemini");
        assert_eq!(proc("/usr/bin/python3 -m aider").program(), "aider");
        assert_eq!(proc("python3.12 -u /x/tool.py").program(), "tool.py");
        assert_eq!(
            proc("/opt/bin/bun run --silent /x/y/cli.js").program(),
            "run"
        );
        assert_eq!(proc("ruby --verbose /x/bin/foo").program(), "foo");
    }

    #[test]
    fn program_plain_and_edge_cases() {
        assert_eq!(
            proc("/Users/x/.local/bin/claude --resume a").program(),
            "claude"
        );
        assert_eq!(proc("claude").program(), "claude");
        assert_eq!(proc("nodemon x").program(), "nodemon");
        assert_eq!(proc("node").program(), "node");
        assert_eq!(proc("node --version").program(), "node");
        assert_eq!(proc("python -m").program(), "python");
        assert_eq!(proc("").program(), "");
    }

    #[test]
    fn program_generic_script_names_use_parent_folder() {
        assert_eq!(proc("node /a/pkg/lib/build/main.js").program(), "pkg");
        assert_eq!(proc("node /a/pkg/cli.js").program(), "pkg");
        assert_eq!(proc("node cli.js").program(), "cli.js");
        assert_eq!(proc("node /dist/index.js").program(), "index.js");
    }

    #[test]
    fn foreground_when_pgid_equals_tpgid() {
        let mut p = proc("x");
        assert!(p.is_foreground());
        p.tpgid = 2;
        assert!(!p.is_foreground());
    }

    #[test]
    fn on_tty_and_get() {
        let t = ProcessTable::parse(
            "1 0 1 1 ttys001 00:01 a\n2 1 2 1 ttys001 00:01 b\n3 1 3 3 ttys002 00:01 c\n4 1 4 0 ?? 00:01 d\n",
        );
        assert_eq!(t.get(3).unwrap().args, "c");
        assert!(t.get(99).is_none());
        let pids: Vec<u32> = t.on_tty("ttys001").iter().map(|p| p.pid).collect();
        assert_eq!(pids, vec![1, 2]);
        assert!(t.on_tty("ttys009").is_empty());
    }

    struct Fake {
        seen: RefCell<Vec<(String, Vec<String>)>>,
        out: Result<String>,
    }

    impl CommandRunner for Fake {
        fn run(&self, program: &str, args: &[&str]) -> Result<String> {
            self.seen.borrow_mut().push((
                program.to_string(),
                args.iter().map(|s| s.to_string()).collect(),
            ));
            match &self.out {
                Ok(s) => Ok(s.clone()),
                Err(e) => Err(crate::error::Error::other(e.to_string())),
            }
        }
    }

    #[test]
    fn load_runs_ps_with_ps_args() {
        let f = Fake {
            seen: RefCell::new(vec![]),
            out: Ok("7 1 7 7 ttys001 00:01 ok\n".into()),
        };
        let t = ProcessTable::load(&f).unwrap();
        assert_eq!(t.procs.len(), 1);
        let seen = f.seen.borrow();
        assert_eq!(seen[0].0, "ps");
        assert_eq!(seen[0].1, PS_ARGS);
    }

    #[test]
    fn load_propagates_errors() {
        let f = Fake {
            seen: RefCell::new(vec![]),
            out: Err(crate::error::Error::other("nope")),
        };
        assert!(ProcessTable::load(&f).is_err());
    }
}
