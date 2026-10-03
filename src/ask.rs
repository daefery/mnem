//! `mnem ask`: a question about past agent work, answered from the record with sources.
//!
//! The memories that best match the question (the same hybrid ranking as MCP search)
//! are shown at once, then a model answers from those memories only, citing them by id;
//! a citation of a memory it was not shown is dropped, so every source printed was
//! really in front of it. Each source says whether the code its session wrote is still
//! there. Without a model, the ranked sources are the answer.

use crate::distill::Llm;
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension};
use serde_json::Value;

/// Memories given to the model, at most.
const SOURCES: usize = 8;

const SYSTEM: &str = r#"You answer a developer's question about their own past work with coding agents,
using only the numbered memories given (notes distilled from their agent sessions, each with an id).
Some memories include "asked:", what the developer typed in that session: the most direct evidence of
why something was done.
Rules: answer in 1-6 short sentences or bullets, plain words, concrete names and values. Cite the
memories you used by id, like [#123], right after the claim they support. State a reason, cause or
motive only when a memory or an "asked:" line states it; never infer one from a later suggestion,
follow-up or next step. If the memories do not answer the question, or do not say why, say so in
one sentence instead of guessing. Never cite an id that was not given.
Return JSON only: {"answer": "...", "cited": [123, 456]}"#;

/// One memory offered as a source.
#[derive(Debug, Clone)]
pub struct Source {
    pub id: i64,
    pub kind: String,
    pub title: String,
    pub created_at: i64,
    /// Whether its session's edits are still in the files it modified, when it can tell.
    pub code: Option<String>,
    text: String,
}

/// The best memories for `question` in `project` (or everywhere), personal details left out.
pub fn sources(conn: &Connection, question: &str, project: Option<&str>) -> Result<Vec<Source>> {
    let filter = "coalesce(m.type, '') != 'sensitive' AND m.kind != 'pinned' AND (? = '' OR m.project = ? OR m.project LIKE ? || '#%')";
    let p = project.unwrap_or("").to_string();
    let args = || -> Vec<Box<dyn rusqlite::ToSql>> {
        vec![
            Box::new(p.clone()),
            Box::new(p.clone()),
            Box::new(p.clone()),
        ]
    };
    let vq = crate::embed::query_from_service(conn, question);
    let ranked = crate::search::rank_memories(conn, question, vq.as_ref(), filter, &args)?;
    let mut st = conn.prepare_cached(
        "SELECT coalesce(type, kind), coalesce(title, ''), coalesce(created_at, 0),
                coalesce(title, '') || ' — ' || coalesce(subtitle, '') || ' — ' || coalesce(narrative, '') || ' Facts: ' || coalesce(facts, '')
           FROM memories WHERE id = ?1",
    )?;
    // What the developer asked in the session behind each memory, from its cited evidence:
    // the direct record of why something was done.
    let mut asked = conn.prepare_cached(
        "SELECT e.text FROM memory_evidence v JOIN events e ON e.id = v.event_id
          WHERE v.memory_id = ?1 AND e.kind = 'prompt' AND e.label IS NULL ORDER BY e.id LIMIT 2",
    )?;
    let mut out = Vec::new();
    for r in ranked.into_iter().take(SOURCES) {
        let row = st
            .query_row([r.id], |x| {
                Ok((
                    x.get::<_, String>(0)?,
                    x.get::<_, String>(1)?,
                    x.get::<_, i64>(2)?,
                    x.get::<_, String>(3)?,
                ))
            })
            .optional()?;
        if let Some((kind, title, created_at, mut text)) = row {
            let prompts: Vec<String> = asked
                .query_map([r.id], |x| x.get::<_, Option<String>>(0))?
                .filter_map(|x| x.ok().flatten())
                .map(|p| crate::text::head(p.trim(), 300).to_string())
                .collect();
            if !prompts.is_empty() {
                text.push_str(&format!(" | asked: \"{}\"", prompts.join("\" / \"")));
            }
            out.push(Source {
                id: r.id,
                kind,
                title,
                created_at,
                code: None,
                text: crate::text::head(&text, 1600).to_string(),
            });
        }
    }
    Ok(out)
}

/// For each source that modified files: whether its own edited lines are still there.
pub fn add_code_state(conn: &Connection, sources: &mut [Source]) {
    for s in sources.iter_mut() {
        if let Ok(lines) = crate::files::staleness_lines(conn, s.id, 2)
            && let Some(first) = lines.first()
        {
            s.code = Some(first.clone());
        }
    }
}

/// The model's answer and the ids it cited that were among `sources`.
pub fn answer(llm: &Llm, question: &str, sources: &[Source]) -> Result<(String, Vec<i64>)> {
    let shown: Vec<String> = sources
        .iter()
        .map(|s| format!("[#{}] ({}) {}", s.id, s.kind, s.text))
        .collect();
    let user = format!("Question: {question}\n\nMemories:\n{}", shown.join("\n"));
    let (v, _model) = llm.ask(SYSTEM, &user)?;
    Ok(read_answer(&v, sources))
}

/// The answer text and its citations, keeping only ids that were shown; citations in
/// the text of ids that were not shown are removed too.
fn read_answer(v: &Value, sources: &[Source]) -> (String, Vec<i64>) {
    let allowed: Vec<i64> = sources.iter().map(|s| s.id).collect();
    let mut text = v["answer"].as_str().unwrap_or_default().trim().to_string();
    let mut cited: Vec<i64> = v["cited"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_i64).collect())
        .unwrap_or_default();
    // Ids the text cites count too, if they were shown.
    for cap in text.match_indices("[#") {
        let rest = &text[cap.0 + 2..];
        if let Some(n) = rest.split(']').next().and_then(|n| n.parse::<i64>().ok()) {
            cited.push(n);
        }
    }
    for bad in cited.iter().filter(|id| !allowed.contains(id)) {
        text = text
            .replace(&format!(" [#{bad}]"), "")
            .replace(&format!("[#{bad}]"), "");
    }
    cited.retain(|id| allowed.contains(id));
    cited.sort_unstable();
    cited.dedup();
    (text, cited)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn src(id: i64) -> Source {
        Source {
            id,
            kind: "decision".into(),
            title: format!("t{id}"),
            created_at: 0,
            code: None,
            text: String::new(),
        }
    }

    #[test]
    fn only_memories_that_were_shown_can_be_cited() {
        let shown = [src(10), src(11)];
        let (text, cited) = read_answer(
            &json!({ "answer": "Backoff was added [#10], then reverted [#99].", "cited": [10, 99, 11] }),
            &shown,
        );
        assert_eq!(cited, vec![10, 11]);
        assert_eq!(text, "Backoff was added [#10], then reverted.");
        // Citations only in the text count when they were shown.
        let (_, cited) = read_answer(&json!({ "answer": "See [#11].", "cited": [] }), &shown);
        assert_eq!(cited, vec![11]);
        let (text, cited) = read_answer(&json!({}), &shown);
        assert!(text.is_empty() && cited.is_empty());
    }
}
