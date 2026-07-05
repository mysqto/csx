//! Canonical domain types shared across sources, index, and search.

use serde::{Deserialize, Serialize};

use crate::error::Error;

/// The AI-coding tool that produced a session transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Tool {
    /// Claude Code CLI transcripts.
    ClaudeCode,
    /// Codex CLI transcripts.
    Codex,
}

impl Tool {
    /// Stable lowercase identifier used in storage and CLI flags.
    pub fn as_str(&self) -> &'static str {
        match self {
            Tool::ClaudeCode => "claude-code",
            Tool::Codex => "codex",
        }
    }

    /// Parse a [`Tool`] from its stable identifier.
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Result<Tool, Error> {
        match s {
            "claude-code" => Ok(Tool::ClaudeCode),
            "codex" => Ok(Tool::Codex),
            other => Err(Error::Unknown(format!("tool: {other}"))),
        }
    }
}

/// The speaker/producer of a single message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Role {
    /// A human user turn.
    User,
    /// An assistant/model turn.
    Assistant,
    /// A tool invocation or tool result turn.
    Tool,
    /// A system prompt or system-level turn.
    System,
}

impl Role {
    /// Stable lowercase identifier used in storage.
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
            Role::System => "system",
        }
    }

    /// Parse a [`Role`] from its stable identifier.
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Result<Role, Error> {
        match s {
            "user" => Ok(Role::User),
            "assistant" => Ok(Role::Assistant),
            "tool" => Ok(Role::Tool),
            "system" => Ok(Role::System),
            other => Err(Error::Unknown(format!("role: {other}"))),
        }
    }
}

/// Account/identity attribution for a session, all fields best-effort.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Account {
    /// The user account UUID, if known.
    pub account_uuid: Option<String>,
    /// The organization UUID, if known.
    pub org_uuid: Option<String>,
    /// The account email, if known.
    pub email: Option<String>,
    /// The organization display name, if known.
    pub org: Option<String>,
}

/// A single normalized message within a session transcript.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageRecord {
    /// The session this message belongs to.
    pub session_id: String,
    /// The tool that produced the session.
    pub tool: Tool,
    /// Monotonic sequence number within the session (0-based).
    pub seq: u64,
    /// Unix timestamp (seconds) of the message.
    pub ts: i64,
    /// Who produced the message.
    pub role: Role,
    /// Name of the tool invoked, when `role` is [`Role::Tool`].
    pub tool_name: Option<String>,
    /// Source-provided message UUID, when available.
    pub uuid: Option<String>,
    /// The extracted plain-text content of the message.
    pub text: String,
    /// Working directory recorded for the message, when available.
    pub cwd: Option<String>,
}

/// Aggregate metadata describing a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMeta {
    /// The session identifier.
    pub session_id: String,
    /// The tool that produced the session.
    pub tool: Tool,
    /// Absolute path of the project the session ran in.
    pub project_path: Option<String>,
    /// Stable identifier for the repository (e.g. remote-derived).
    pub repo_id: Option<String>,
    /// Human-friendly project name.
    pub project_name: Option<String>,
    /// Git branch active during the session, if known.
    pub git_branch: Option<String>,
    /// Account attribution, if resolved.
    pub account: Option<Account>,
    /// Unix timestamp (seconds) of the earliest message.
    pub first_ts: i64,
    /// Unix timestamp (seconds) of the latest message.
    pub last_ts: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_roundtrip() {
        for t in [Tool::ClaudeCode, Tool::Codex] {
            assert_eq!(Tool::from_str(t.as_str()).unwrap(), t);
        }
        assert!(Tool::from_str("nope").is_err());
    }

    #[test]
    fn role_roundtrip() {
        for r in [Role::User, Role::Assistant, Role::Tool, Role::System] {
            assert_eq!(Role::from_str(r.as_str()).unwrap(), r);
        }
        let err = Role::from_str("ghost").unwrap_err();
        assert!(err.to_string().contains("role: ghost"));
    }

    #[test]
    fn account_default_is_empty() {
        let a = Account::default();
        assert!(a.account_uuid.is_none());
        assert!(a.email.is_none());
    }

    #[test]
    fn records_serialize_roundtrip() {
        let msg = MessageRecord {
            session_id: "s1".into(),
            tool: Tool::Codex,
            seq: 3,
            ts: 1_700_000_000,
            role: Role::Assistant,
            tool_name: None,
            uuid: Some("u-1".into()),
            text: "hello".into(),
            cwd: Some("/proj".into()),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let back: MessageRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, back);

        let meta = SessionMeta {
            session_id: "s1".into(),
            tool: Tool::Codex,
            project_path: Some("/proj".into()),
            repo_id: Some("r1".into()),
            project_name: Some("proj".into()),
            git_branch: Some("main".into()),
            account: Some(Account {
                account_uuid: Some("a".into()),
                org_uuid: None,
                email: Some("dev@example.com".into()),
                org: None,
            }),
            first_ts: 1,
            last_ts: 2,
        };
        let json = serde_json::to_string(&meta).unwrap();
        let back: SessionMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(meta, back);
    }
}
