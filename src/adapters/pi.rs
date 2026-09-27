//! pi.dev transcripts: ~/.pi/agent/sessions/<cwd-slug>/<ts>_<id>.jsonl (session format v3).

use super::{Emit, content_text, str_of};
use crate::model::Kind;
use crate::text;
use serde_json::Value;

pub fn line(em: &mut Emit, v: &Value) {
    let id = str_of(v, "id").unwrap_or_default().to_string();
    match str_of(v, "type") {
        Some("session") => {
            em.st.session_id.get_or_insert_with(|| id.clone());
            if let Some(c) = str_of(v, "cwd") {
                em.st.cwd = Some(c.to_string());
            }
        }
        Some("compaction") => {
            if let Some(s) = str_of(v, "summary") {
                em.push(format!("{id}:compact"), Kind::Compaction, text::clean(s, 6000));
            }
        }
        Some("message") => message(em, &id, v.get("message").unwrap_or(&Value::Null)),
        _ => {}
    }
}

fn message(em: &mut Emit, id: &str, m: &Value) {
    let content = m.get("content").unwrap_or(&Value::Null);
    match str_of(m, "role") {
        Some("user") => em.prompt(format!("{id}:prompt"), &content_text(content, &["text"])),
        Some("assistant") => {
            let Value::Array(parts) = content else { return };
            for (i, p) in parts.iter().enumerate() {
                match str_of(p, "type") {
                    Some("text") => em.assistant(format!("{id}:{i}"), str_of(p, "text").unwrap_or_default()),
                    Some("toolCall") => {
                        let name = str_of(p, "name").unwrap_or("?");
                        let args = p.get("arguments").unwrap_or(&Value::Null);
                        em.tool_call(format!("{id}:{i}"), str_of(p, "id"), name, args);
                    }
                    _ => {}
                }
            }
        }
        Some("toolResult") => {
            let call = str_of(m, "toolCallId");
            if m.get("isError").and_then(Value::as_bool) == Some(true) {
                em.tool_error(format!("{id}:err"), call, &content_text(content, &["text"]));
            }
            em.tool_done(call);
        }
        // User-run `!cmd` shell executions.
        Some("bashExecution") => {
            let cmd = str_of(m, "command").unwrap_or_default();
            let e = em.push(format!("{id}:bash"), Kind::Command, text::clean(cmd, 600));
            e.tool = Some("bash".into());
            let code = m.get("exitCode").and_then(Value::as_i64).unwrap_or(0);
            if code != 0 {
                let out = str_of(m, "output").unwrap_or_default();
                let e = em.push(format!("{id}:err"), Kind::Error, text::redact(&text::tail(out, 12, 800)));
                e.is_error = true;
                e.tool = Some("bash".into());
            }
        }
        _ => {}
    }
}
