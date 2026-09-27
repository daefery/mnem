//! Codex CLI transcripts: ~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl
//!
//! The same message can appear in several envelopes (`event_msg/*`, `item_completed`,
//! `response_item/*`). `response_item` is skipped entirely; messages are keyed by
//! (turn, text hash) so the remaining duplicates collapse on insert.

use super::{Emit, content_text, str_of};
use crate::model::Kind;
use crate::text;
use serde_json::Value;

pub fn line(em: &mut Emit, v: &Value) {
    let p = v.get("payload").unwrap_or(&Value::Null);
    match (str_of(v, "type"), str_of(p, "type")) {
        (Some("session_meta"), _) => {
            if let Some(id) = str_of(p, "id").or_else(|| str_of(p, "session_id")) {
                em.st.session_id.get_or_insert_with(|| id.to_string());
            }
            if let Some(c) = str_of(p, "cwd") {
                em.st.cwd = Some(c.to_string());
            }
            if let Some(g) = p.get("git") {
                if let Some(u) = str_of(g, "repository_url") {
                    em.st.repo_url = Some(u.to_string());
                }
                if let Some(b) = str_of(g, "branch") {
                    em.st.git_branch = Some(b.to_string());
                }
            }
        }
        (Some("turn_context"), _) => {
            if let Some(c) = str_of(p, "cwd") {
                em.st.cwd = Some(c.to_string());
            }
        }
        (Some("event_msg"), Some("task_started")) => {
            em.st.turn += 1;
            em.st.turn_id = str_of(p, "turn_id").map(str::to_string);
        }
        (Some("event_msg"), Some("user_message")) => {
            prompt(em, None, str_of(p, "message").unwrap_or_default());
        }
        (Some("event_msg"), Some("agent_message")) => {
            assistant(em, None, str_of(p, "message").unwrap_or_default());
        }
        (Some("event_msg"), Some("task_complete")) => {
            assistant(em, str_of(p, "turn_id"), str_of(p, "last_agent_message").unwrap_or_default());
        }
        (Some("event_msg"), Some("item_completed")) => {
            if let Some(item) = p.get("item") {
                self::item(em, str_of(p, "turn_id"), item);
            }
        }
        _ => {}
    }
}

fn turn_key(em: &Emit, turn_id: Option<&str>) -> String {
    turn_id
        .map(str::to_string)
        .or_else(|| em.st.turn_id.clone())
        .unwrap_or_else(|| em.st.turn.to_string())
}

fn prompt(em: &mut Emit, turn_id: Option<&str>, raw: &str) {
    let t = raw.trim();
    if t.is_empty() {
        return;
    }
    let key = format!("u:{}:{}", turn_key(em, turn_id), text::hash(t));
    em.push(key, Kind::Prompt, text::clean(t, 4000));
}

fn assistant(em: &mut Emit, turn_id: Option<&str>, raw: &str) {
    let t = raw.trim();
    if t.is_empty() {
        return;
    }
    let key = format!("a:{}:{}", turn_key(em, turn_id), text::hash(t));
    em.assistant(key, t);
}

fn item(em: &mut Emit, turn_id: Option<&str>, it: &Value) {
    let id = str_of(it, "id").unwrap_or_default().to_string();
    let content = it.get("content").unwrap_or(&Value::Null);
    match str_of(it, "type") {
        Some("UserMessage") => prompt(em, turn_id, &content_text(content, &["text"])),
        Some("AgentMessage") => assistant(em, turn_id, &content_text(content, &["Text", "text"])),
        Some("CommandExecution") => {
            let cmd = match it.get("command") {
                // ["/bin/bash", "-lc", "<script>"]: the script is what matters.
                Some(Value::Array(a)) => a.last().and_then(Value::as_str).unwrap_or_default().to_string(),
                Some(Value::String(s)) => s.clone(),
                _ => return,
            };
            let e = em.push(id.clone(), Kind::Command, text::clean(&cmd, 600));
            e.tool = Some("exec".into());
            let code = it.get("exit_code").and_then(Value::as_i64).unwrap_or(0);
            if code != 0 {
                let out = str_of(it, "aggregated_output").or_else(|| str_of(it, "stderr")).unwrap_or_default();
                let msg = format!("exit {code}: {}", text::head(&cmd, 120));
                let e = em.push(format!("{id}:err"), Kind::Error, text::redact(&format!("{msg}\n{}", text::tail(out, 12, 800))));
                e.is_error = true;
                e.tool = Some("exec".into());
            }
        }
        Some("FileChange") => {
            if let Some(Value::Object(changes)) = it.get("changes") {
                for path in changes.keys() {
                    let e = em.push(format!("{id}:{}", text::hash(path)), Kind::FileEdit, String::new());
                    e.tool = Some("apply_patch".into());
                    e.path = Some(path.clone());
                }
            }
            if str_of(it, "status") == Some("failed") {
                let out = str_of(it, "stderr").unwrap_or_default();
                let e = em.push(format!("{id}:err"), Kind::Error, text::redact(&text::tail(out, 12, 800)));
                e.is_error = true;
                e.tool = Some("apply_patch".into());
            }
        }
        Some("McpToolCall") => {
            let name = format!(
                "mcp__{}__{}",
                str_of(it, "server").unwrap_or("?"),
                str_of(it, "tool").unwrap_or("?")
            );
            em.tool_call(id, None, &name, it.get("arguments").unwrap_or(&Value::Null));
        }
        _ => {}
    }
}
