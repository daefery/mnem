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
            thread: self.st.thread.clone(),
            label: None,
            tool_raw: None,
        });
        self.out.last_mut().expect("just pushed")
    }

    /// Record a user prompt that starts a new turn.
    pub fn prompt(&mut self, key: String, raw: &str) {
        self.prompt_with(key, raw, true);
    }

    /// Record a user prompt. Wrappers are dropped, tooling-injected prompts are labelled
    /// "harness", and a prompt identical to the previous one (polling loops) is skipped.
    pub fn prompt_with(&mut self, key: String, raw: &str, new_turn: bool) {
        let Some((t, label)) = classify_prompt(raw) else {
            return;
        };
        let h = text::hash(&t);
        if self.st.last_prompt.as_deref() == Some(h.as_str()) {
            return;
        }
        self.st.last_prompt = Some(h);
        if new_turn {
            self.st.turn += 1;
        }
        let e = self.push(key, Kind::Prompt, text::clean(&t, 4000));
        e.label = label.map(str::to_string);
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
        let canon = canonical_tool(name);
        let s = |k: &str| input.get(k).and_then(Value::as_str);
        let path = s("file_path")
            .or_else(|| s("path"))
            .or_else(|| s("notebook_path"));
        let (kind, text) = match canon.as_str() {
            "shell" => match s("command").or_else(|| s("cmd")) {
                Some(c) => (Kind::Command, text::clean(c, 600)),
                None => return,
            },
            "read" => (Kind::FileRead, String::new()),
            "edit" | "write" => (Kind::FileEdit, String::new()),
            n if SKIP_TOOLS.contains(&n) => return,
            _ => (Kind::Tool, text::clean(&brief(input), 300)),
        };
        if matches!(kind, Kind::FileRead | Kind::FileEdit) && path.is_none() {
            return;
        }
        let e = self.push(key, kind, text);
        e.tool = Some(canon);
        e.tool_raw = Some(name.to_string());
        e.path = path.map(str::to_string);
    }

    /// Record a failed tool result, labelled with the originating tool when known.
    pub fn tool_error(&mut self, key: String, id: Option<&str>, output: &str) {
        let raw = id.and_then(|i| self.st.pending.get(i).cloned());
        self.error(key, raw.as_deref(), "", output);
    }

    /// Record an error with a class label. Permission denials are not project facts and
    /// are dropped. `context` (e.g. "exit 2: make test") is prepended when non-empty.
    pub fn error(&mut self, key: String, tool_raw: Option<&str>, context: &str, output: &str) {
        let Some(class) = classify_error(output) else {
            return;
        };
        let body = text::clean_error(output, 2, 4, 800);
        let text = if context.is_empty() {
            body
        } else {
            format!("{}\n{body}", text::clean(context, 200))
        };
        let e = self.push(key, Kind::Error, text);
        e.is_error = true;
        e.label = Some(class.to_string());
        e.tool = tool_raw.map(canonical_tool);
        e.tool_raw = tool_raw.map(str::to_string);
    }

    pub fn tool_done(&mut self, id: Option<&str>) {
        if let Some(id) = id {
            self.st.pending.remove(id);
        }
    }
}

/// One vocabulary across agents: Claude "Bash", pi "bash" and Codex "exec" are all "shell".
pub fn canonical_tool(name: &str) -> String {
    let l = name.to_ascii_lowercase();
    match l.as_str() {
        "bash" | "shell" | "exec" | "exec_command" | "local_shell" => "shell".into(),
        "edit" | "multiedit" | "notebookedit" | "apply_patch" | "str_replace" => "edit".into(),
        "write" | "create" => "write".into(),
        "read" | "view" | "cat" => "read".into(),
        _ => l,
    }
}

/// Prompt text worth keeping, with an optional label; None for pure harness wrappers.
pub fn classify_prompt(raw: &str) -> Option<(String, Option<&'static str>)> {
    let t = text::strip_pasted(raw.trim());
    let t = t.trim();
    if t.is_empty() || t.starts_with("[Request interrupted") {
        return None;
    }
    // Orchestrators mark their messages with an invisible separator (U+2063), wrap task
    // files, or replay delegated history. They are real instructions, just not typed by a human.
    if let Some(rest) = t.strip_prefix('\u{2063}') {
        return Some((rest.trim().to_string(), Some("harness")));
    }
    if t.starts_with("<file name=") || t.starts_with("The following is the Codex agent history") {
        return Some((t.to_string(), Some("harness")));
    }
    if !t.starts_with('<') {
        return Some((t.to_string(), None));
    }
    // Slash commands: <command-name>/foo</command-name> ... <command-args>bar</command-args>
    let tag = |name: &str| {
        let open = format!("<{name}>");
        let close = format!("</{name}>");
        let s = t.find(&open)? + open.len();
        let e = t[s..].find(&close)? + s;
        Some(t[s..e].trim().to_string())
    };
    let name = tag("command-name")?;
    let args = tag("command-args").unwrap_or_default();
    Some((format!("{name} {args}").trim().to_string(), None))
}

/// Error class, or None when the "error" is a permission denial rather than a failure.
pub fn classify_error(out: &str) -> Option<&'static str> {
    let l = out.to_ascii_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|n| l.contains(n));
    if has(&[
        "doesn't want to proceed with this tool use",
        "tool use was rejected",
        "permission to use",
        "attempt to accomplish this action using other tools",
        "user denied",
        "was blocked by",
    ]) {
        return None;
    }
    Some(
        if has(&[
            "string to replace not found",
            "could not find edits",
            "old_string",
            "no changes to make",
            "patch failed",
            "failed to apply",
        ]) {
            "edit_mismatch"
        } else if has(&[
            "test result: failed",
            "tests failed",
            "assertionerror",
            "failures:",
            "short test summary",
            "npm err! test",
            "✗",
            " failed",
        ]) {
            "test_fail"
        } else if has(&[
            "no such file or directory",
            "command not found",
            "not found",
        ]) {
            "not_found"
        } else if has(&["timed out", "timeout"]) {
            "timeout"
        } else if has(&["error[e", "error:", "exception", "traceback"]) {
            "error"
        } else {
            "nonzero_exit"
        },
    )
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
            .filter(|p| {
                p.get("type")
                    .and_then(Value::as_str)
                    .is_some_and(|t| types.contains(&t))
            })
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

#[cfg(test)]
mod tests {
    use super::{canonical_tool, classify_error, classify_prompt};

    #[test]
    fn prompts() {
        assert_eq!(
            classify_prompt("fix the bug"),
            Some(("fix the bug".into(), None))
        );
        assert_eq!(
            classify_prompt("<task-notification>x</task-notification>"),
            None
        );
        assert_eq!(
            classify_prompt("<command-name>/model</command-name><command-args>opus</command-args>"),
            Some(("/model opus".into(), None))
        );
        assert_eq!(
            classify_prompt("\u{2063}OP: do x"),
            Some(("OP: do x".into(), Some("harness")))
        );
        assert_eq!(
            classify_prompt("look <pasted_content>huge dump</pasted_content>"),
            Some(("look [pasted 9 chars]".into(), None))
        );
    }

    #[test]
    fn tools_and_errors() {
        assert_eq!(canonical_tool("Bash"), "shell");
        assert_eq!(canonical_tool("apply_patch"), "edit");
        assert_eq!(
            classify_error("The user doesn't want to proceed with this tool use."),
            None
        );
        assert_eq!(
            classify_error("test result: FAILED. 1 passed; 2 failed"),
            Some("test_fail")
        );
        assert_eq!(
            classify_error("<tool_use_error>String to replace not found</tool_use_error>"),
            Some("edit_mismatch")
        );
    }
}
