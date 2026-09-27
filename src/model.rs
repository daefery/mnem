use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Agent {
    Claude,
    Codex,
    Pi,
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
}

/// Per-file parser state. Persisted with the cursor so ingest can resume mid-file.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ParserState {
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
}
