use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Agent {
    Claude,
    Codex,
    Pi,
}

impl std::str::FromStr for Agent {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "claude" | "claude-code" => Ok(Agent::Claude),
            "codex" => Ok(Agent::Codex),
            "pi" => Ok(Agent::Pi),
            other => Err(format!(
                "unknown agent {other:?} (expected claude, codex or pi)"
            )),
        }
    }
}

impl Agent {
    pub fn as_str(self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
            Agent::Pi => "pi",
        }
    }
}

/// Normalized event kinds shared by every agent adapter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Prompt,
    Assistant,
    Title,
    Recap,
    Compaction,
    Command,
    FileRead,
    FileEdit,
    Tool,
    Error,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Prompt => "prompt",
            Kind::Assistant => "assistant",
            Kind::Title => "title",
            Kind::Recap => "recap",
            Kind::Compaction => "compaction",
            Kind::Command => "command",
            Kind::FileRead => "file_read",
            Kind::FileEdit => "file_edit",
            Kind::Tool => "tool",
            Kind::Error => "error",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Event {
    /// Stable per-session dedupe key derived from the agent's own record ids.
    pub key: String,
    pub ts: i64,
    pub turn: i64,
    pub kind: Kind,
    pub tool: Option<String>,
    pub path: Option<String>,
    pub text: String,
    pub is_error: bool,
    pub byte_offset: u64,
    /// Subagent thread (e.g. "agent-a1b2"); None for the main conversation.
    pub thread: Option<String>,
    /// Sub-classification: prompt "harness" (injected by tooling), error class, ...
    pub label: Option<String>,
    /// Tool name as the agent spelled it; `tool` holds the canonical name.
    pub tool_raw: Option<String>,
}

/// Bump when ParserState changes meaning; older persisted states force a replay from 0.
pub const STATE_VERSION: u32 = 3;

/// Per-file parser state. Persisted with the cursor so ingest can resume mid-file.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ParserState {
    #[serde(default)]
    pub v: u32,
    pub session_id: Option<String>,
    pub cwd: Option<String>,
    pub git_branch: Option<String>,
    pub repo_url: Option<String>,
    pub title: Option<String>,
    pub started_at: Option<i64>,
    pub last_ts: i64,
    pub turn: i64,
    pub turn_id: Option<String>,
    /// tool call id -> tool name, for labelling error results.
    pub pending: HashMap<String, String>,
    #[serde(default)]
    pub thread: Option<String>,
    /// Hashes of recent harness prompts, to drop polling repeats.
    #[serde(default)]
    pub recent_harness: Vec<String>,
}

impl Default for ParserState {
    fn default() -> Self {
        Self {
            v: STATE_VERSION,
            session_id: None,
            cwd: None,
            git_branch: None,
            repo_url: None,
            title: None,
            started_at: None,
            last_ts: 0,
            turn: 0,
            turn_id: None,
            pending: HashMap::new(),
            thread: None,
            recent_harness: Vec::new(),
        }
    }
}
