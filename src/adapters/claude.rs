//! Claude Code transcripts: ~/.claude/projects/<project>/<session>.jsonl
//! (plus <session>/subagents/agent-*.jsonl, which share the parent sessionId).

use super::{Emit, content_text, str_of};
use crate::model::Kind;
use crate::text;
use serde_json::Value;

pub fn line(em: &mut Emit, v: &Value) {
    if let Some(s) = str_of(v, "sessionId") {
        em.st.session_id.get_or_insert_with(|| s.to_string());
    }
    if let Some(c) = str_of(v, "cwd") {
        em.st.cwd = Some(c.to_string());
    }
    if let Some(b) = str_of(v, "gitBranch").filter(|b| !b.is_empty()) {
        em.st.git_branch = Some(b.to_string());
    }
    let uuid = str_of(v, "uuid").unwrap_or_default().to_string();
    let side = v
        .get("isSidechain")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let msg = v.get("message").unwrap_or(&Value::Null);
    match str_of(v, "type") {
        Some("user") => user(em, v, msg, &uuid, side),
        Some("assistant") => assistant(em, v, msg, &uuid, side),
        Some("system") if str_of(v, "subtype") == Some("away_summary") => {
            if let Some(c) = str_of(v, "content") {
                em.push(format!("recap:{uuid}"), Kind::Recap, text::clean(c, 2000));
            }
        }
        Some("ai-title") => {
            if let Some(t) = str_of(v, "aiTitle") {
                em.st.title = Some(t.to_string());
                em.push(
                    format!("title:{}", text::hash(t)),
                    Kind::Title,
                    text::clean(t, 200),
                );
            }
        }
        _ => {}
    }
}

fn user(em: &mut Emit, v: &Value, msg: &Value, uuid: &str, side: bool) {
    let content = msg.get("content").unwrap_or(&Value::Null);
    if v.get("isCompactSummary").and_then(Value::as_bool) == Some(true) {
        let t = content_text(content, &["text"]);
        em.push(
            format!("{uuid}:compact"),
            Kind::Compaction,
            text::clean(&t, 6000),
        );
        return;
    }
    if let Value::Array(parts) = content {
        for (i, p) in parts.iter().enumerate() {
            if str_of(p, "type") != Some("tool_result") {
                continue;
            }
            let id = str_of(p, "tool_use_id");
            if p.get("is_error").and_then(Value::as_bool) == Some(true) {
                let out = content_text(p.get("content").unwrap_or(&Value::Null), &["text"]);
                em.tool_error(format!("{uuid}:{i}:err"), id, &out);
            }
            em.tool_done(id);
        }
    }
    if side || v.get("isMeta").and_then(Value::as_bool) == Some(true) {
        return;
    }
    em.prompt(format!("{uuid}:prompt"), &content_text(content, &["text"]));
}

fn assistant(em: &mut Emit, v: &Value, msg: &Value, uuid: &str, side: bool) {
    if v.get("isApiErrorMessage").and_then(Value::as_bool) == Some(true) {
        return;
    }
    let Some(Value::Array(parts)) = msg.get("content") else {
        return;
    };
    for (i, p) in parts.iter().enumerate() {
        match str_of(p, "type") {
            Some("text") if !side => {
                em.assistant(format!("{uuid}:{i}"), str_of(p, "text").unwrap_or_default());
            }
            Some("tool_use") => {
                let name = str_of(p, "name").unwrap_or("?");
                let input = p.get("input").unwrap_or(&Value::Null);
                em.tool_call(format!("{uuid}:{i}"), str_of(p, "id"), name, input);
            }
            _ => {}
        }
    }
}
