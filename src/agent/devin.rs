//! Devin CLI agent detection.
//!
//! Devin writes `<devin_home>/session_locks/<session id>.lock` holding the pid
//! of the `devin` process that owns the session. Locks of exited processes
//! stay behind, so a lock only links a process to a session when its pid
//! matches. Session details come from `<devin_home>/sessions.db`, opened
//! read-only.

use std::path::Path;
use std::time::UNIX_EPOCH;

use super::{base_info, shell_quote, Agent, AgentContext};
use crate::source::devin::open_readonly;
use crate::tabs::process::Process;
use crate::tabs::AgentInfo;

/// Devin CLI.
pub struct DevinAgent;

/// True for a Devin session name such as `swamp-movie`: lowercase letters and
/// digits in words joined by single hyphens. Only such ids go into a resume
/// command.
fn is_session_name(s: &str) -> bool {
    s.split('-').all(|w| {
        !w.is_empty()
            && w.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
    })
}

/// The session whose lock file holds `pid`. Locks of exited processes stay
/// behind, so when a reused pid matches several locks the most recently
/// written one wins.
fn session_for_pid(locks: &Path, pid: u32) -> Option<String> {
    let want = pid.to_string();
    std::fs::read_dir(locks)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|path| path.extension().is_some_and(|x| x == "lock"))
        .filter(|path| std::fs::read_to_string(path).is_ok_and(|t| t.trim() == want))
        .filter_map(|path| {
            let mtime = std::fs::metadata(&path)
                .and_then(|m| m.modified())
                .unwrap_or(UNIX_EPOCH);
            Some((mtime, path.file_stem()?.to_str()?.to_string()))
        })
        .max()
        .map(|(_, id)| id)
}

/// What the database says about one session.
struct SessionRow {
    title: Option<String>,
    cwd: String,
    last_activity_secs: i64,
    last_prompt: Option<String>,
}

/// Read one session row and its newest typed prompt (shell commands are
/// skipped). `None` when the database or the row is missing or unreadable.
fn read_session(db: &Path, id: &str) -> Option<SessionRow> {
    let conn = open_readonly(db).ok()?;
    let (title, cwd, last_activity_secs) = conn
        .query_row(
            "SELECT title, working_directory, last_activity_at FROM sessions WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .ok()?;
    let last_prompt = conn
        .query_row(
            "SELECT content FROM prompt_history
             WHERE session_id = ?1 AND is_shell = 0
             ORDER BY timestamp DESC, id DESC LIMIT 1",
            [id],
            |r| r.get(0),
        )
        .ok();
    Some(SessionRow {
        title,
        cwd,
        last_activity_secs,
        last_prompt,
    })
}

impl Agent for DevinAgent {
    fn tool(&self) -> &'static str {
        "devin"
    }

    fn matches(&self, p: &Process) -> bool {
        p.program() == "devin"
    }

    fn enrich(&self, p: &Process, ctx: &AgentContext) -> AgentInfo {
        let mut info = base_info(self.tool(), p);
        let Some(id) = session_for_pid(&ctx.devin_home.join("session_locks"), p.pid) else {
            return info;
        };
        let Some(row) = read_session(&ctx.devin_home.join("sessions.db"), &id) else {
            return info;
        };
        info.resume_command =
            is_session_name(&id).then(|| format!("cd {} && devin -r {id}", shell_quote(&row.cwd)));
        info.session_id = Some(id);
        info.title = row.title;
        info.cwd = Some(row.cwd);
        info.updated_at = Some(row.last_activity_secs * 1000);
        info.last_prompt = row.last_prompt;
        info
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::devin::tests::{add_session, fixture_db};
    use crate::tabs::AgentStatus;
    use rusqlite::Connection;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime};

    fn proc(pid: u32, args: &str) -> Process {
        Process {
            pid,
            ppid: 1,
            pgid: pid,
            tpgid: pid,
            tty: Some("ttys7".into()),
            elapsed_secs: 5,
            args: args.into(),
        }
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("csx-devin-agent-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(d.join("session_locks")).unwrap();
        d
    }

    fn ctx(home: &Path) -> AgentContext {
        AgentContext {
            home: PathBuf::from("/nonexistent"),
            claude_dirs: vec![],
            codex_home: PathBuf::from("/nonexistent"),
            devin_home: home.to_path_buf(),
            now_ms: 0,
        }
    }

    fn lock(home: &Path, id: &str, body: &str, age_secs: u64) {
        let path = home.join(format!("session_locks/{id}.lock"));
        fs::write(&path, body).unwrap();
        let mtime = SystemTime::now() - Duration::from_secs(age_secs);
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
    }

    fn prompt(conn: &Connection, sid: &str, content: &str, ts: i64, shell: i64) {
        conn.execute(
            "INSERT INTO prompt_history (content, timestamp, session_id, is_shell)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![content, ts, sid, shell],
        )
        .unwrap();
    }

    fn bare(pid: u32) -> AgentInfo {
        AgentInfo {
            tool: "devin".into(),
            pid,
            tty: Some("ttys7".into()),
            ..AgentInfo::default()
        }
    }

    #[test]
    fn devin_matches_by_program() {
        assert!(DevinAgent.matches(&proc(1, "devin")));
        assert!(DevinAgent.matches(&proc(
            1,
            "/h/.local/share/devin/cli/_versions/3000.11.205/bin/devin -r swamp-movie"
        )));
        assert!(!DevinAgent.matches(&proc(1, "devinctl")));
        assert!(!DevinAgent.matches(&proc(1, "vim devin.txt")));
        assert_eq!(DevinAgent.tool(), "devin");
    }

    #[test]
    fn session_name_validation() {
        for ok in ["swamp-movie", "a", "abc-123-x9", "42"] {
            assert!(is_session_name(ok), "{ok}");
        }
        for bad in [
            "",
            "-a",
            "a-",
            "a--b",
            "Swamp-movie",
            "a_b",
            "a b",
            "x; rm -rf ~",
            "../x",
        ] {
            assert!(!is_session_name(bad), "{bad}");
        }
    }

    #[test]
    fn devin_reads_lock_session_row_and_last_prompt() {
        let h = tmp("full");
        let conn = fixture_db(&h);
        add_session(
            &conn,
            "swift-otter",
            "/work/acme/it's here",
            1_700_000_000,
            0,
        );
        add_session(&conn, "quiet-fox", "/work/acme/lib", 1_700_000_100, 0);
        prompt(&conn, "swift-otter", "first prompt", 10, 0);
        prompt(&conn, "swift-otter", "newest prompt", 30, 0);
        // A newer shell command is not a prompt; nor is another session's.
        prompt(&conn, "swift-otter", "ls -la", 40, 1);
        prompt(&conn, "quiet-fox", "other session", 50, 0);
        drop(conn);
        lock(&h, "swift-otter", "4242\n", 0);
        // A stale lock of an exited process and a non-lock file are ignored.
        lock(&h, "quiet-fox", "999", 0);
        fs::write(h.join("session_locks/notes.txt"), "4242").unwrap();

        let info = DevinAgent.enrich(&proc(4242, "devin"), &ctx(&h));
        assert_eq!(info.tool, "devin");
        assert_eq!(info.pid, 4242);
        assert_eq!(info.tty.as_deref(), Some("ttys7"));
        assert_eq!(info.session_id.as_deref(), Some("swift-otter"));
        assert_eq!(info.title.as_deref(), Some("a title"));
        assert_eq!(info.cwd.as_deref(), Some("/work/acme/it's here"));
        assert_eq!(info.updated_at, Some(1_700_000_000_000));
        assert_eq!(info.last_prompt.as_deref(), Some("newest prompt"));
        assert_eq!(info.status, AgentStatus::Unknown);
        assert_eq!(
            info.resume_command.as_deref(),
            Some("cd '/work/acme/it'\\''s here' && devin -r swift-otter")
        );
    }

    #[test]
    fn devin_newest_lock_wins_when_a_stale_one_has_the_same_pid() {
        let h = tmp("reuse");
        let conn = fixture_db(&h);
        add_session(&conn, "old-run", "/work/acme/a", 1, 0);
        add_session(&conn, "new-run", "/work/acme/b", 2, 0);
        drop(conn);
        lock(&h, "old-run", "77", 3_600);
        lock(&h, "new-run", "77", 0);
        let info = DevinAgent.enrich(&proc(77, "devin"), &ctx(&h));
        assert_eq!(info.session_id.as_deref(), Some("new-run"));
        assert_eq!(info.last_prompt, None);
    }

    #[test]
    fn devin_without_lock_db_or_row_has_tool_pid_tty_only() {
        // No lock directory at all.
        let h = tmp("nolock");
        fs::remove_dir_all(h.join("session_locks")).unwrap();
        assert_eq!(DevinAgent.enrich(&proc(5, "devin"), &ctx(&h)), bare(5));
        // A lock but no database.
        let h = tmp("nodb");
        lock(&h, "swift-otter", "5", 0);
        assert_eq!(DevinAgent.enrich(&proc(5, "devin"), &ctx(&h)), bare(5));
        // A database without that session row.
        drop(fixture_db(&h));
        assert_eq!(DevinAgent.enrich(&proc(5, "devin"), &ctx(&h)), bare(5));
    }

    #[test]
    fn devin_resume_command_needs_a_valid_session_name() {
        let h = tmp("crafted");
        let conn = fixture_db(&h);
        add_session(&conn, "Bad Name", "/work/acme/c", 1, 0);
        drop(conn);
        lock(&h, "Bad Name", "8", 0);
        let info = DevinAgent.enrich(&proc(8, "devin"), &ctx(&h));
        assert_eq!(info.session_id.as_deref(), Some("Bad Name"));
        assert_eq!(info.cwd.as_deref(), Some("/work/acme/c"));
        assert_eq!(info.resume_command, None);
    }

    #[test]
    fn devin_tolerates_a_database_without_prompt_history_or_with_bad_rows() {
        let h = tmp("partial");
        let conn = Connection::open(h.join("sessions.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, working_directory TEXT NOT NULL,
                                    last_activity_at INTEGER NOT NULL, title TEXT);
             INSERT INTO sessions VALUES ('swift-otter', '/work/acme/a', 3, NULL);
             INSERT INTO sessions VALUES ('blob-cwd', X'00', 3, NULL);",
        )
        .unwrap();
        drop(conn);
        lock(&h, "swift-otter", "9", 0);
        let info = DevinAgent.enrich(&proc(9, "devin"), &ctx(&h));
        assert_eq!(info.session_id.as_deref(), Some("swift-otter"));
        assert_eq!(info.title, None);
        assert_eq!(info.last_prompt, None);
        assert_eq!(info.updated_at, Some(3_000));
        // A row whose columns have the wrong type is treated as missing.
        lock(&h, "blob-cwd", "10", 0);
        assert_eq!(DevinAgent.enrich(&proc(10, "devin"), &ctx(&h)), bare(10));
    }
}
