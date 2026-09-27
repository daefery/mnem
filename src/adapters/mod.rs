//! Transcript adapters: one JSONL line in, zero or more normalized events out.
//!
//! Adapters are pure functions over (state, line). All cross-line context lives in
//! `ParserState`, which is persisted with the cursor so ingest can resume mid-file.

mod claude;
mod codex;
mod pi;

use crate::model::{Agent, Event, Kind, ParserState};
use crate::text;
use serde_json::Value;

pub struct Emit<'a> {
    pub st: &'a mut ParserState,
    pub out: &'a mut Vec<Event>,
    pub off: u64,
    pub ts: i64,
}

impl Emit<'_> {
    pub fn push(&mut self, key: String, kind: Kind, text: String) -> &mut Event {
        self.out.push(Event {
            key,
            ts: self.ts,
            turn: self.st.turn,
            kind,
            tool: None,
            path: None,
            text,
            is_error: false,
            byte_offset: self.off,
        });
        self.out.last_mut().expect("just pushed")
    }

    pub fn prompt(&mut self, key: String, raw: &str) {
        let t = raw.trim();
        if t.is_empty() {
            return;
        }
        self.st.turn += 1;
        self.push(key, Kind::Prompt, text::clean(t, 4000));
    }

    pub fn assistant(&mut self, key: String, raw: &str) {
        let t = raw.trim();
        if !t.is_empty() {
            self.push(key, Kind::Assistant, text::clean(t, 4000));
        }
    }

    /// Record a tool call. Noise tools (search, todo, sleep...) are skipped.
    pub fn tool_call(&mut self, key: String, id: Option<&str>, name: &str, input: &Value) {
        if let Some(id) = id {
            self.st.pending.insert(id.to_string(), name.to_string());
        }
        let lname = name.to_ascii_lowercase();
        let s = |k: &str| input.get(k).and_then(Value::as_str);
        let path = s("file_path").or_else(|| s("path")).or_else(|| s("notebook_path"));
        let (kind, text) = match lname.as_str() {
            "bash" | "shell" | "exec_command" => match s("command").or_else(|| s("cmd")) {
                Some(c) => (Kind::Command, text::clean(c, 600)),
                None => return,
            },
            "read" => (Kind::FileRead, String::new()),
            "edit" | "write" | "multiedit" | "notebookedit" => (Kind::FileEdit, String::new()),
            n if SKIP_TOOLS.contains(&n) => return,
            _ => (Kind::Tool, text::clean(&brief(input), 300)),
        };
        if matches!(kind, Kind::FileRead | Kind::FileEdit) && path.is_none() {
            return;
        }
        let e = self.push(key, kind, text);
        e.tool = Some(name.to_string());
        e.path = path.map(str::to_string);
    }

    /// Record a failed tool result, labelled with the originating tool when known.
    pub fn tool_error(&mut self, key: String, id: Option<&str>, output: &str) {
        let tool = id.and_then(|i| self.st.pending.get(i).cloned());
        let e = self.push(key, Kind::Error, text::redact(&text::tail(output, 12, 800)));
        e.is_error = true;
        e.tool = tool;
    }

    pub fn tool_done(&mut self, id: Option<&str>) {
        if let Some(id) = id {
            self.st.pending.remove(id);
        }
    }
}

const SKIP_TOOLS: &[&str] = &[
    "todowrite",
    "toolsearch",
    "listmcpresourcestool",
    "askuserquestion",
    "exitplanmode",
    "enterplanmode",
    "wait",
    "sleep",
    "grep",
    "glob",
    "find",
    "ls",
    "skill",
    "slashcommand",
];

fn brief(input: &Value) -> String {
    match input {
        Value::Object(m) => m
            .iter()
            .filter_map(|(k, v)| {
                let s = match v {
                    Value::String(s) => text::head(s, 80),
                    Value::Number(_) | Value::Bool(_) => v.to_string(),
                    _ => return None,
                };
                Some(format!("{k}={s}"))
            })
            .collect::<Vec<_>>()
            .join(" "),
        Value::String(s) => text::head(s, 200),
        _ => String::new(),
    }
}

/// Join text parts from a content array (`[{type: text, text}]`) or a plain string.
pub fn content_text(v: &Value, types: &[&str]) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter(|p| p.get("type").and_then(Value::as_str).is_some_and(|t| types.contains(&t)))
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

pub fn str_of<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str)
}

/// Parse one line. Err means the line was not valid JSON and belongs in quarantine.
pub fn parse_line(
    agent: Agent,
    st: &mut ParserState,
    line: &str,
    off: u64,
    out: &mut Vec<Event>,
) -> Result<(), String> {
    let v: Value = serde_json::from_str(line).map_err(|e| e.to_string())?;
    let ts = v
        .get("timestamp")
        .and_then(|t| t.as_str().and_then(text::parse_ts).or_else(|| t.as_i64()))
        .unwrap_or(st.last_ts);
    if ts > 0 {
        st.last_ts = st.last_ts.max(ts);
        st.started_at.get_or_insert(ts);
    }
    let mut em = Emit { st, out, off, ts };
    match agent {
        Agent::Claude => claude::line(&mut em, &v),
        Agent::Codex => codex::line(&mut em, &v),
        Agent::Pi => pi::line(&mut em, &v),
    }
    Ok(())
}
