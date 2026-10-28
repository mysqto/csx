//! `csx tabs open`: resume an indexed session in a new terminal tab, or focus
//! the tab that already runs it. A plugin names only the tool and the session
//! id; the folder and the command come from the index.

use super::app::{self, process_list_failed, ListOptions};
use super::{Ports, TerminalBackend};
use crate::agent::{
    is_safe_typed_path, resume_line, session_env, Agent, AgentContext, RESUMABLE_TOOLS,
};
use crate::db::Db;
use crate::error::{Error, Result};
use std::path::Path;

/// What `open_session` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenOutcome {
    /// An agent already ran the session; csx focused its tab, by this ref.
    Focused(String),
    /// csx opened a new tab that resumes the session.
    Opened,
}

/// Resume session `session_id` of `tool` in a new tab of `terminal`.
///
/// Checks, in order: the terminal is one of `backends`, the tool can be
/// resumed, the index has the session, its folder is absolute and exists, the
/// folder and config folder can be typed safely, and the id passes the tool's
/// check. Then it lists the agents again: when one
/// runs this session in a tab, csx focuses that tab and opens nothing. When
/// the process list cannot be read, csx refuses, because it cannot tell.
// The brief fixes this signature: the ports, the two registries, the agent
// context, the index and the three values the plugin passes.
#[allow(clippy::too_many_arguments)]
pub fn open_session(
    ports: &Ports,
    backends: &[Box<dyn TerminalBackend>],
    agents: &[Box<dyn Agent>],
    ctx: &AgentContext,
    db: &Db,
    terminal: &str,
    tool: &str,
    session_id: &str,
) -> Result<OpenOutcome> {
    let backend = backends
        .iter()
        .find(|b| b.id() == terminal)
        .ok_or_else(|| Error::other(format!("unknown terminal: {terminal}")))?;
    if !RESUMABLE_TOOLS.contains(&tool) {
        return Err(Error::other(format!("cannot resume {tool} sessions")));
    }
    let no_session = || Error::other(format!("no such session: {tool} {session_id}"));
    let session = db
        .resumable_session(tool, session_id)?
        .ok_or_else(no_session)?;
    let cwd = session.project_path.as_deref().ok_or_else(no_session)?;
    let path = Path::new(cwd);
    if !path.is_absolute() || !path.is_dir() {
        return Err(Error::other(format!("the folder {cwd} no longer exists")));
    }
    if !is_safe_typed_path(cwd) {
        return Err(Error::other(format!(
            "the folder {cwd} cannot be typed safely"
        )));
    }
    let env = session_env(tool, session.config_dir.as_deref(), &ctx.home);
    if let Some((_, dir)) = env.iter().find(|(_, v)| !is_safe_typed_path(v)) {
        return Err(Error::other(format!(
            "the config folder {dir} cannot be typed safely"
        )));
    }
    let line = resume_line(tool, session_id, cwd, &env)
        .ok_or_else(|| Error::other(format!("invalid session id: {session_id}")))?;
    let report = app::list(ports, backends, agents, ctx, &ListOptions::default());
    if process_list_failed(&report) {
        return Err(Error::other("could not read the process list"));
    }
    let live = report.rows.iter().find(|r| {
        r.agent
            .as_ref()
            .is_some_and(|a| a.session_id.as_deref() == Some(session_id))
    });
    if let Some(tab_ref) = live.and_then(|r| r.ref_.clone()) {
        app::focus(ports, backends, &tab_ref)?;
        return Ok(OpenOutcome::Focused(tab_ref));
    }
    backend.open_tab(ports, cwd, &line)?;
    Ok(OpenOutcome::Opened)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::SourceRow;
    use crate::model::{SessionMeta, Tool};
    use crate::tabs::test_support::{
        ctx, log, ps_line, tab, with_tty, FakeAgent, FakeBackend, FakeOs, Log,
    };
    use crate::tabs::AgentInfo;
    use std::fs;
    use std::path::PathBuf;

    const U1: &str = "11111111-1111-4111-8111-111111111111";
    const U2: &str = "22222222-2222-4222-8222-222222222222";

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("csx-open-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// An index with one session of `tool` in `cwd`, from a source whose
    /// config folder is `config_dir`.
    fn db_with(tool: Tool, id: &str, cwd: Option<&str>, config_dir: Option<&str>) -> Db {
        let db = Db::open(":memory:").unwrap();
        let src = db
            .upsert_source(&SourceRow {
                tool: Some(tool.as_str().into()),
                config_dir: config_dir.map(str::to_string),
                ..SourceRow::default()
            })
            .unwrap();
        let meta = SessionMeta {
            session_id: id.into(),
            tool,
            project_path: cwd.map(str::to_string),
            repo_id: None,
            project_name: None,
            git_branch: None,
            account: None,
            first_ts: 1,
            last_ts: 2,
        };
        db.upsert_session(src, &meta, 0, None).unwrap();
        db
    }

    fn running(pid: u32, session_id: &str) -> AgentInfo {
        AgentInfo {
            tool: "codex".into(),
            pid,
            session_id: Some(session_id.into()),
            ..AgentInfo::default()
        }
    }

    struct Rig {
        log: Log,
        os: FakeOs,
        backends: Vec<Box<dyn TerminalBackend>>,
        agents: Vec<Box<dyn Agent>>,
    }

    /// No agent runs; `ps` answers with one unrelated process.
    fn rig() -> Rig {
        let l = log();
        Rig {
            os: FakeOs::new(&l).answer("ps -axo", &ps_line(5, "ttys005")),
            backends: vec![
                FakeBackend::new("ghostty", &l).boxed(),
                FakeBackend::new("iterm2", &l).boxed(),
            ],
            agents: vec![Box::new(FakeAgent { infos: vec![] })],
            log: l,
        }
    }

    impl Rig {
        fn open(&self, db: &Db, terminal: &str, tool: &str, id: &str) -> Result<OpenOutcome> {
            open_session(
                &self.os.ports(),
                &self.backends,
                &self.agents,
                &ctx(),
                db,
                terminal,
                tool,
                id,
            )
        }

        fn opened(&self) -> Vec<String> {
            self.log
                .borrow()
                .iter()
                .filter(|e| e.starts_with("open:"))
                .cloned()
                .collect()
        }
    }

    fn err(r: Result<OpenOutcome>) -> String {
        r.unwrap_err().to_string()
    }

    #[test]
    fn opens_a_new_tab_with_the_resume_line() {
        let root = tmp("ok");
        let cwd = root.join("it's here");
        fs::create_dir_all(&cwd).unwrap();
        let cwd = cwd.to_str().unwrap();
        let db = db_with(Tool::Codex, U1, Some(cwd), None);
        let r = rig();
        assert_eq!(
            r.open(&db, "iterm2", "codex", U1).unwrap(),
            OpenOutcome::Opened
        );
        let quoted = cwd.replace('\'', "'\\''");
        assert_eq!(
            r.opened(),
            [format!(
                "open:iterm2:{cwd}:cd '{quoted}' && codex resume {U1}"
            )]
        );
        // The guard read the process list before the tab opened.
        let log = r.log.borrow();
        let ps = log.iter().position(|e| e.starts_with("run:ps ")).unwrap();
        assert!(ps < log.iter().position(|e| e.starts_with("open:")).unwrap());
    }

    #[test]
    fn sets_the_config_variable_for_a_non_default_folder() {
        let root = tmp("env");
        let cwd = root.to_str().unwrap();
        let db = db_with(Tool::ClaudeCode, U1, Some(cwd), Some("/cfg/.claude-b"));
        let r = rig();
        r.open(&db, "ghostty", "claude-code", U1).unwrap();
        assert_eq!(
            r.opened(),
            [format!(
                "open:ghostty:{cwd}:cd '{cwd}' && CLAUDE_CONFIG_DIR='/cfg/.claude-b' claude --resume {U1}"
            )]
        );
    }

    #[test]
    fn focuses_the_tab_that_already_runs_the_session() {
        let root = tmp("guard");
        let db = db_with(Tool::Codex, U1, root.to_str(), None);
        let l = log();
        let os = FakeOs::new(&l).answer("ps -axo", &ps_line(10, "ttys002"));
        let backends = vec![
            FakeBackend::new("ghostty", &l).boxed(),
            FakeBackend::new("iterm2", &l)
                .tabs(vec![with_tty(
                    tab("iterm2", "iterm2:b", "codex", 1, 1),
                    "ttys002",
                )])
                .boxed(),
        ];
        let agents: Vec<Box<dyn Agent>> = vec![Box::new(FakeAgent {
            infos: vec![running(10, U1)],
        })];
        let r = Rig {
            log: l,
            os,
            backends,
            agents,
        };
        assert_eq!(
            r.open(&db, "ghostty", "codex", U1).unwrap(),
            OpenOutcome::Focused("iterm2:b".into())
        );
        assert!(r
            .log
            .borrow()
            .contains(&"focus:iterm2:iterm2:b".to_string()));
        assert!(r.opened().is_empty());
    }

    #[test]
    fn another_running_session_does_not_stop_the_open() {
        let root = tmp("other");
        let db = db_with(Tool::Codex, U1, root.to_str(), None);
        let l = log();
        let r = Rig {
            os: FakeOs::new(&l).answer("ps -axo", &ps_line(10, "ttys002")),
            backends: vec![FakeBackend::new("ghostty", &l).boxed()],
            agents: vec![Box::new(FakeAgent {
                infos: vec![running(10, U2)],
            })],
            log: l,
        };
        assert_eq!(
            r.open(&db, "ghostty", "codex", U1).unwrap(),
            OpenOutcome::Opened
        );
    }

    #[test]
    fn a_failed_focus_is_an_error_and_opens_nothing() {
        let root = tmp("focus-fail");
        let db = db_with(Tool::Codex, U1, root.to_str(), None);
        let l = log();
        // The agent has no tab, so its ref is `tty:ttys009`, which no backend
        // shows.
        let r = Rig {
            os: FakeOs::new(&l).answer("ps -axo", &ps_line(10, "ttys009")),
            backends: vec![FakeBackend::new("ghostty", &l).boxed()],
            agents: vec![Box::new(FakeAgent {
                infos: vec![running(10, U1)],
            })],
            log: l,
        };
        let e = err(r.open(&db, "ghostty", "codex", U1));
        assert!(
            e.contains("no supported terminal shows /dev/ttys009"),
            "{e}"
        );
        assert!(r.opened().is_empty());
    }

    #[test]
    fn refuses_when_the_process_list_cannot_be_read() {
        let root = tmp("ps");
        let db = db_with(Tool::Codex, U1, root.to_str(), None);
        let l = log();
        let r = Rig {
            os: FakeOs::new(&l),
            backends: vec![FakeBackend::new("ghostty", &l).boxed()],
            agents: vec![Box::new(FakeAgent { infos: vec![] })],
            log: l,
        };
        assert_eq!(
            err(r.open(&db, "ghostty", "codex", U1)),
            "could not read the process list"
        );
        assert!(r.opened().is_empty());
    }

    #[test]
    fn a_failed_open_is_an_error() {
        let root = tmp("open-fail");
        let db = db_with(Tool::Codex, U1, root.to_str(), None);
        let l = log();
        let r = Rig {
            os: FakeOs::new(&l).answer("ps -axo", ""),
            backends: vec![FakeBackend::new("ghostty", &l).open_error().boxed()],
            agents: vec![Box::new(FakeAgent { infos: vec![] })],
            log: l,
        };
        assert_eq!(err(r.open(&db, "ghostty", "codex", U1)), "cannot script");
    }

    #[test]
    fn unknown_terminal_and_tool() {
        let root = tmp("unknown");
        let db = db_with(Tool::Codex, U1, root.to_str(), None);
        let r = rig();
        assert_eq!(
            err(r.open(&db, "tmux", "codex", U1)),
            "unknown terminal: tmux"
        );
        // A terminal csx supports but that is not among the backends.
        assert_eq!(
            err(r.open(&db, "kitty", "codex", U1)),
            "unknown terminal: kitty"
        );
        assert_eq!(
            err(r.open(&db, "ghostty", "gemini", U1)),
            "cannot resume gemini sessions"
        );
        // Nothing was listed or run.
        assert!(r.log.borrow().is_empty());
    }

    #[test]
    fn unknown_session() {
        let root = tmp("nosession");
        let db = db_with(Tool::Codex, U1, root.to_str(), None);
        let r = rig();
        assert_eq!(
            err(r.open(&db, "ghostty", "codex", U2)),
            format!("no such session: codex {U2}")
        );
        // The id is looked up for its own tool only.
        assert_eq!(
            err(r.open(&db, "ghostty", "devin", U1)),
            format!("no such session: devin {U1}")
        );
        // A session with no folder cannot be resumed.
        let db = db_with(Tool::Codex, U1, None, None);
        assert_eq!(
            err(r.open(&db, "ghostty", "codex", U1)),
            format!("no such session: codex {U1}")
        );
        let broken = Db::open(":memory:").unwrap();
        broken.conn().execute_batch("DROP TABLE sessions;").unwrap();
        assert!(r.open(&broken, "ghostty", "codex", U1).is_err());
        assert!(r.log.borrow().is_empty());
    }

    #[test]
    fn missing_or_relative_folder() {
        let root = tmp("folder");
        let gone = root.join("gone");
        let gone = gone.to_str().unwrap();
        let r = rig();
        let db = db_with(Tool::Codex, U1, Some(gone), None);
        assert_eq!(
            err(r.open(&db, "ghostty", "codex", U1)),
            format!("the folder {gone} no longer exists")
        );
        // `src` exists relative to the test's folder; it is still refused.
        for rel in ["src", "-rf"] {
            let db = db_with(Tool::Codex, U1, Some(rel), None);
            assert_eq!(
                err(r.open(&db, "ghostty", "codex", U1)),
                format!("the folder {rel} no longer exists")
            );
        }
        assert!(r.log.borrow().is_empty());
    }

    #[test]
    fn refuses_folders_and_config_dirs_that_cannot_be_typed_safely() {
        let root = tmp("unsafe");
        let r = rig();
        for name in ["a\x03b", "a\nb", "a\rb", "a\x1bb", "trailing\\", "mid\\dle"] {
            let dir = root.join(name);
            fs::create_dir_all(&dir).unwrap();
            let dir = dir.to_str().unwrap();
            let db = db_with(Tool::Codex, U1, Some(dir), None);
            assert_eq!(
                err(r.open(&db, "ghostty", "codex", U1)),
                format!("the folder {dir} cannot be typed safely")
            );
            let ok = root.to_str().unwrap();
            let db = db_with(Tool::Codex, U1, Some(ok), Some(dir));
            assert_eq!(
                err(r.open(&db, "ghostty", "codex", U1)),
                format!("the config folder {dir} cannot be typed safely")
            );
        }
        assert!(r.log.borrow().is_empty());
        // Quotes, `$` and backticks are typed inside single quotes.
        let odd = root.join("it's $HOME `x`");
        fs::create_dir_all(&odd).unwrap();
        let odd = odd.to_str().unwrap();
        let db = db_with(Tool::Codex, U1, Some(odd), None);
        assert_eq!(
            r.open(&db, "ghostty", "codex", U1).unwrap(),
            OpenOutcome::Opened
        );
        let quoted = odd.replace('\'', "'\\''");
        assert_eq!(
            r.opened(),
            [format!(
                "open:ghostty:{odd}:cd '{quoted}' && codex resume {U1}"
            )]
        );
    }

    #[test]
    fn invalid_session_id() {
        let root = tmp("badid");
        let r = rig();
        let db = db_with(Tool::Devin, "Bad Id; rm -rf ~", root.to_str(), None);
        assert_eq!(
            err(r.open(&db, "ghostty", "devin", "Bad Id; rm -rf ~")),
            "invalid session id: Bad Id; rm -rf ~"
        );
        let db = db_with(Tool::Codex, "not-a-uuid", root.to_str(), None);
        assert_eq!(
            err(r.open(&db, "ghostty", "codex", "not-a-uuid")),
            "invalid session id: not-a-uuid"
        );
        assert!(r.log.borrow().is_empty());
    }
}
