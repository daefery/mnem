//! `rvn trace`: Agent Trace records (agent-trace.dev, v0.1.0) for git commits, built
//! from the transcripts ravnori already holds, so attribution covers commits made before any
//! tracing tool was installed.
//!
//! A line a commit adds is attributed to an agent session when that exact line (trimmed,
//! 12 characters or more, as the edit check reads lines) is among the lines the session
//! wrote to that file with an edit tool call in the LOOKBACK before the commit was made.
//! Times are author dates and the later of author and committer date: a rebase or merge
//! train moves the committer date hours past the edits, the author date stays. Only lines
//! the commit adds count, so an older edit of the same line cannot be counted twice. Shorter lines (braces, blanks) between two lines of one session join its
//! range. Matching is exact on purpose: code a formatter rewrote is left unattributed,
//! so counts are a floor; a line a person typed that an agent had also just written is
//! the one way to over-attribute.

use anyhow::{Context, Result, bail};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// The Agent Trace version these records follow.
pub const SPEC: &str = "0.1.0";
/// Lines shorter than this (trimmed) say nothing about who wrote them.
const MIN_LINE: usize = 12;
/// How long before a commit an agent's edit may have been made to count for it.
const LOOKBACK_MS: i64 = 14 * 86_400_000;

/// One line an agent wrote to a file, with who and when.
#[derive(Clone)]
struct Written {
    session: String,
    ts: i64,
    model: Option<String>,
}

/// Every agent edit to files in `root`, by repo-relative path: (time, session, lines
/// written, model), read back from the transcript record each edit came from.
type Edits = HashMap<String, Vec<(i64, String, Vec<String>, Option<String>)>>;

/// A file_edit event as stored: where to read the edit back from.
struct EditEvent {
    session: String,
    ts: i64,
    path: String,
    key: String,
    source: String,
    offset: i64,
    cwd: Option<String>,
}

/// One session's attributed ranges in a file.
struct Conversation {
    session: String,
    model: Option<String>,
    ranges: Vec<(u64, u64)>,
}

/// Agents often work in other checkouts of the repository (git worktrees, a second
/// clone); their sessions share its project id, so edits are found by project, and each
/// path is made relative to the root of the checkout the session worked in.
fn agent_edits(conn: &Connection, root: &Path) -> Result<Edits> {
    let r = root.to_string_lossy().to_string();
    let project = crate::project::Resolver::default().resolve(Some(&r), None);
    let base = project
        .as_deref()
        .map(|p| p.split('#').next().unwrap_or(p).to_string());
    let mut st = conn.prepare(
        "SELECT e.session_id, coalesce(e.ts, 0), e.path, e.record_key, e.source_path, coalesce(e.byte_offset, 0), s.cwd
           FROM events e JOIN sessions s ON s.id = e.session_id
          WHERE e.kind = 'file_edit' AND e.path IS NOT NULL AND e.source_path IS NOT NULL
            AND (e.path = ?1 OR e.path LIKE ?1 || '/%' OR s.cwd = ?1 OR s.cwd LIKE ?1 || '/%'
                 OR (?2 != '' AND (s.project = ?2 OR s.project LIKE ?2 || '#%')))
          ORDER BY e.id",
    )?;
    let mut roots: HashMap<String, Option<PathBuf>> = HashMap::new();
    let rows: Vec<EditEvent> = st
        .query_map(rusqlite::params![&r, base.as_deref().unwrap_or("")], |x| {
            Ok(EditEvent {
                session: x.get(0)?,
                ts: x.get(1)?,
                path: x.get(2)?,
                key: x.get(3)?,
                source: x.get(4)?,
                offset: x.get(5)?,
                cwd: x.get(6)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    let mut models: HashMap<String, Option<String>> = HashMap::new();
    let mut out: Edits = HashMap::new();
    for EditEvent {
        session,
        ts,
        path,
        key,
        source,
        offset,
        cwd,
    } in rows
    {
        let abs = if Path::new(&path).is_absolute() {
            PathBuf::from(&path)
        } else if let Some(c) = &cwd {
            Path::new(c).join(&path)
        } else {
            continue;
        };
        // Relative to this repository, or else to the checkout the session worked in.
        let checkout = cwd.as_ref().and_then(|c| {
            roots
                .entry(c.clone())
                .or_insert_with(|| crate::files::repo_root(Path::new(c)))
                .clone()
        });
        let Some(rel) = abs
            .strip_prefix(root)
            .ok()
            .or_else(|| checkout.as_deref().and_then(|ch| abs.strip_prefix(ch).ok()))
        else {
            continue;
        };
        let rel = rel.to_string_lossy().trim_start_matches("./").to_string();
        let Some(record) = crate::files::record_at(Path::new(&source), offset) else {
            continue;
        };
        let Some(edit) = crate::files::edit_at(&record, &key, &path) else {
            continue;
        };
        if edit.new.is_empty() {
            continue;
        }
        let model = record
            .pointer("/message/model")
            .and_then(Value::as_str)
            .map(model_id)
            .or_else(|| {
                models
                    .entry(source.clone())
                    .or_insert_with(|| transcript_model(Path::new(&source)))
                    .clone()
            });
        out.entry(rel)
            .or_default()
            .push((ts, session, edit.new, model));
    }
    Ok(out)
}

/// The model a Codex transcript names in its session or turn context (its edit records
/// do not carry one).
fn transcript_model(path: &Path) -> Option<String> {
    use std::io::{BufRead, BufReader};
    let f = std::fs::File::open(path).ok()?;
    for line in BufReader::new(f).lines().take(400).map_while(Result::ok) {
        if !(line.contains("session_meta") || line.contains("turn_context")) {
            continue;
        }
        let v: Value = serde_json::from_str(&line).ok()?;
        if let Some(m) = v.pointer("/payload/model").and_then(Value::as_str) {
            return Some(model_id(m));
        }
    }
    None
}

/// A model name in the models.dev form (`anthropic/claude-…`, `openai/gpt-…`), routing
/// prefixes such as a proxy's `developer/` dropped.
pub fn model_id(m: &str) -> String {
    let base = m.rsplit('/').next().unwrap_or(m);
    let provider = if base.starts_with("claude") {
        Some("anthropic")
    } else if base.starts_with("gpt")
        || base.starts_with("codex")
        || base.starts_with('o') && base[1..].starts_with(|c: char| c.is_ascii_digit())
    {
        Some("openai")
    } else if base.starts_with("gemini") {
        Some("google")
    } else {
        None
    };
    match provider {
        Some(p) => format!("{p}/{base}"),
        None => m.to_string(),
    }
}

fn git(root: &Path, args: &[&str]) -> Result<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["-c", "core.quotePath=false"])
        .args(args)
        .output()
        .context("run git")?;
    if !out.status.success() {
        bail!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Lines a commit added, per file: (line number in the new file, text).
fn added_lines(
    root: &Path,
    sha: &str,
    parent: Option<&str>,
) -> Result<HashMap<String, Vec<(u64, String)>>> {
    // The empty tree stands in for the parent of a first commit.
    let base = parent.unwrap_or("4b825dc642cb6eb9a060e54bf8d69288fbee4904");
    let diff = git(
        root,
        &[
            "diff",
            "-U0",
            "--no-color",
            "--no-renames",
            "--no-ext-diff",
            base,
            sha,
        ],
    )?;
    let mut out: HashMap<String, Vec<(u64, String)>> = HashMap::new();
    let (mut file, mut line): (Option<String>, u64) = (None, 0);
    for l in diff.lines() {
        if let Some(p) = l.strip_prefix("+++ ") {
            file = p.strip_prefix("b/").map(str::to_string);
        } else if let Some(h) = l.strip_prefix("@@ ") {
            // @@ -a,b +c,d @@
            line = h
                .split_whitespace()
                .find_map(|t| t.strip_prefix('+'))
                .and_then(|t| t.split(',').next())
                .and_then(|n| n.parse().ok())
                .unwrap_or(0);
        } else if let (Some(f), Some(text)) = (&file, l.strip_prefix('+')) {
            out.entry(f.clone())
                .or_default()
                .push((line, text.to_string()));
            line += 1;
        }
    }
    Ok(out)
}

/// Who wrote each added line: Some(session) for an agent, None for a long line no agent
/// wrote, and short lines take the session of the attributed lines around them.
fn attribute(added: &[(u64, String)], written: &HashMap<String, Written>) -> Vec<Option<Written>> {
    #[derive(Clone)]
    enum Who {
        Agent(Written),
        Person,
        Short,
    }
    let who: Vec<Who> = added
        .iter()
        .map(|(_, t)| {
            let t = t.trim();
            if t.chars().count() < MIN_LINE {
                Who::Short
            } else {
                match written.get(t) {
                    Some(w) => Who::Agent(w.clone()),
                    None => Who::Person,
                }
            }
        })
        .collect();
    let mut out: Vec<Option<Written>> = Vec::with_capacity(who.len());
    for i in 0..who.len() {
        out.push(match &who[i] {
            Who::Agent(w) => Some(w.clone()),
            Who::Person => None,
            Who::Short => {
                // The nearest decided lines on each side, within one block of added lines.
                let contiguous = |a: usize, b: usize| added[a].0 + 1 == added[b].0;
                let mut before = None;
                let mut j = i;
                while j > 0 && contiguous(j - 1, j) {
                    j -= 1;
                    if !matches!(who[j], Who::Short) {
                        before = Some(&who[j]);
                        break;
                    }
                }
                let mut after = None;
                let mut k = i;
                while k + 1 < who.len() && contiguous(k, k + 1) {
                    k += 1;
                    if !matches!(who[k], Who::Short) {
                        after = Some(&who[k]);
                        break;
                    }
                }
                match (before, after) {
                    (Some(Who::Agent(a)), Some(Who::Agent(b))) if a.session == b.session => {
                        Some(a.clone())
                    }
                    _ => None,
                }
            }
        });
    }
    out
}

/// A stable UUID for a commit's record, so exporting twice gives the same id. The seed
/// keeps ravnori's earlier name: records already exported keep their ids.
fn record_id(sha: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut b: [u8; 16] = Sha256::digest(format!("mnem-agent-trace:{sha}").as_bytes())[..16]
        .try_into()
        .expect("16 bytes");
    b[6] = (b[6] & 0x0f) | 0x50;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    )
}

/// A summary of what an export found.
#[derive(Default, Debug)]
pub struct Totals {
    pub commits: usize,
    pub added: usize,
    pub attributed: usize,
}

/// Agent Trace records for `commits` (oldest first) in the repository at `root`.
pub fn records(conn: &Connection, root: &Path, commits: &[String]) -> Result<(Vec<Value>, Totals)> {
    let edits = agent_edits(conn, root)?;
    let mut totals = Totals::default();
    let mut out = Vec::new();
    for sha in commits {
        let info = git(root, &["show", "-s", "--format=%H %at %ct %P", sha])?;
        let mut f = info.split_whitespace();
        let full = f.next().unwrap_or(sha).to_string();
        let mut num = || f.next().and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);
        let (authored, committed) = (num(), num());
        let parent = f.next().map(str::to_string);
        let secs = authored;
        let (from, to) = (
            authored * 1000 - LOOKBACK_MS,
            authored.max(committed) * 1000,
        );
        let added = added_lines(root, &full, parent.as_deref())?;
        let mut files = Vec::new();
        let (mut n_added, mut n_ai) = (0usize, 0usize);
        let mut paths: Vec<&String> = added.keys().collect();
        paths.sort();
        for path in paths {
            let lines = &added[path];
            n_added += lines.len();
            let mut written: HashMap<String, Written> = HashMap::new();
            for (ts, session, new, model) in edits.get(path).into_iter().flatten() {
                if *ts < from || *ts > to {
                    continue;
                }
                for l in new {
                    // The latest edit before the commit wins.
                    let w = Written {
                        session: session.clone(),
                        ts: *ts,
                        model: model.clone(),
                    };
                    match written.get(l) {
                        Some(old) if old.ts > *ts => {}
                        _ => {
                            written.insert(l.clone(), w);
                        }
                    }
                }
            }
            let who = attribute(lines, &written);
            // Ranges per session: consecutive line numbers with the same author.
            let mut convs: Vec<Conversation> = Vec::new();
            for (i, w) in who.iter().enumerate() {
                let Some(w) = w else { continue };
                n_ai += 1;
                let line = lines[i].0;
                let c = match convs.iter_mut().find(|c| c.session == w.session) {
                    Some(c) => c,
                    None => {
                        convs.push(Conversation {
                            session: w.session.clone(),
                            model: w.model.clone(),
                            ranges: Vec::new(),
                        });
                        convs.last_mut().expect("just pushed")
                    }
                };
                match c.ranges.last_mut() {
                    Some(r) if r.1 + 1 == line => r.1 = line,
                    _ => c.ranges.push((line, line)),
                }
            }
            if convs.is_empty() {
                continue;
            }
            files.push(json!({
                "path": path,
                "conversations": convs.iter().map(|Conversation { session, model, ranges }| {
                    let mut contributor = json!({ "type": "ai" });
                    if let Some(m) = model {
                        contributor["model_id"] = json!(m);
                    }
                    json!({
                        "url": format!("ravnori://session/{session}"),
                        "contributor": contributor,
                        "ranges": ranges.iter().map(|(a, b)| json!({ "start_line": a, "end_line": b })).collect::<Vec<_>>(),
                    })
                }).collect::<Vec<_>>(),
            }));
        }
        let timestamp: String = conn.query_row(
            "SELECT strftime('%Y-%m-%dT%H:%M:%SZ', ?1, 'unixepoch')",
            [secs],
            |r| r.get(0),
        )?;
        totals.commits += 1;
        totals.added += n_added;
        totals.attributed += n_ai;
        out.push(json!({
            "version": SPEC,
            "id": record_id(&full),
            "timestamp": timestamp,
            "vcs": { "type": "git", "revision": full },
            "tool": { "name": "ravnori", "version": env!("CARGO_PKG_VERSION") },
            "files": files,
            "metadata": { "dev.ravnori": {
                "added_lines": n_added,
                "attributed_lines": n_ai,
                "method": "exact match of the lines the commit added (12+ characters) against lines agents wrote to the file with edit tool calls in the 14 days before it, read from their transcripts; files written by shell commands are not seen, so this is a floor",
            } },
        }));
    }
    Ok((out, totals))
}

/// The commits to trace, oldest first: those after `since`, or the last `n`; merges skipped.
pub fn commits(root: &Path, since: Option<&str>, n: usize) -> Result<Vec<String>> {
    let list = match since {
        Some(s) => git(
            root,
            &[
                "rev-list",
                "--no-merges",
                "--reverse",
                &format!("{s}..HEAD"),
            ],
        )?,
        None => git(
            root,
            &[
                "rev-list",
                "--no-merges",
                "--reverse",
                "-n",
                &n.to_string(),
                "HEAD",
            ],
        )?,
    };
    Ok(list.lines().map(str::to_string).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;

    fn run(root: &Path, env: &[(&str, String)], args: &[&str]) {
        let ok = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .envs(env.iter().map(|(k, v)| (*k, v.as_str())))
            .output()
            .unwrap();
        assert!(
            ok.status.success(),
            "{}",
            String::from_utf8_lossy(&ok.stderr)
        );
    }

    #[test]
    fn commits_are_attributed_from_agent_edits_in_transcripts() {
        let dir = crate::TempDir::new("trace");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        run(&dir, &[], &["init", "-q"]);
        let file = dir.join("src/a.rs");
        let agent1 = "    let agent_wrote_this = compute();";
        let agent2 = "    let second_agent_line = more_work();";
        let human = "    let human_wrote_this = 1;";
        // Transcript: three agent edits (Claude Code records), the last after commit 1.
        let transcript = dir.join("t.jsonl");
        let mut body = String::new();
        let mut offsets = Vec::new();
        for (i, new) in [
            format!("fn main() {{\n{agent1}\n}}"),
            agent2.to_string(),
            human.to_string(),
        ]
        .iter()
        .enumerate()
        {
            offsets.push(body.len() as i64);
            body.push_str(&json!({ "uuid": format!("u{i}"), "message": { "model": "claude-opus-5-5", "content": [
                { "type": "tool_use", "input": { "file_path": file, "old_string": "", "new_string": new } }] } }).to_string());
            body.push('\n');
        }
        std::fs::write(&transcript, body).unwrap();
        let c =
            crate::db::open_with(Path::new(":memory:"), std::time::Duration::from_secs(1)).unwrap();
        c.execute(
            "INSERT INTO sessions(id, agent, native_id, cwd) VALUES ('claude:s1', 'claude', 's1', ?1)",
            [dir.to_string_lossy()],
        )
        .unwrap();
        let t1 = 1_800_000_000i64; // commit 1, seconds
        let t2 = t1 + 3600; // commit 2
        // Edit times (ms): before commit 1, between the commits, and between them again
        // (the human line, which the person also typed: written after commit 1 only).
        for (i, ts) in [(t1 - 60) * 1000, (t1 + 60) * 1000, (t2 + 60) * 1000]
            .iter()
            .enumerate()
        {
            c.execute(
                "INSERT INTO events(session_id, record_key, kind, path, ts, source_path, byte_offset)
                 VALUES ('claude:s1', ?1, 'file_edit', ?2, ?3, ?4, ?5)",
                params![format!("u{i}:0"), file.to_string_lossy(), ts, transcript.to_string_lossy(), offsets[i]],
            )
            .unwrap();
        }
        let date = |s: i64| {
            vec![
                ("GIT_AUTHOR_DATE", format!("@{s} +0000")),
                ("GIT_COMMITTER_DATE", format!("@{s} +0000")),
            ]
        };
        std::fs::write(&file, format!("fn main() {{\n{agent1}\n{human}\n}}\n")).unwrap();
        run(&dir, &date(t1), &["add", "src/a.rs"]);
        run(&dir, &date(t1), &["commit", "-q", "-m", "one"]);
        std::fs::write(
            &file,
            format!("fn main() {{\n{agent1}\n{human}\n{agent2}\n}}\n"),
        )
        .unwrap();
        // Rebased by a merge train: committed hours after it was written.
        let rebased = vec![
            ("GIT_AUTHOR_DATE", format!("@{t2} +0000")),
            ("GIT_COMMITTER_DATE", format!("@{} +0000", t2 + 6 * 3600)),
        ];
        run(&dir, &rebased, &["commit", "-qam", "two"]);

        let list = commits(&dir, None, 10).unwrap();
        assert_eq!(list.len(), 2);
        let (recs, totals) = records(&c, &dir, &list).unwrap();
        // Commit 1: line 2 is the agent's; the human line (written by the agent only
        // after this commit) and the braces are not.
        let f = &recs[0]["files"][0];
        assert_eq!(f["path"], "src/a.rs");
        let conv = &f["conversations"][0];
        assert_eq!(conv["contributor"]["model_id"], "anthropic/claude-opus-5-5");
        assert_eq!(conv["url"], "ravnori://session/claude:s1");
        assert_eq!(conv["ranges"], json!([{ "start_line": 2, "end_line": 2 }]));
        assert_eq!(recs[0]["metadata"]["dev.ravnori"]["added_lines"], 4);
        assert_eq!(recs[0]["metadata"]["dev.ravnori"]["attributed_lines"], 1);
        // Commit 2 (rebased, committed 6 h later) added line 4, written by the agent
        // between the commits; line 2, written before commit 1, is not added again.
        assert_eq!(
            recs[1]["files"][0]["conversations"][0]["ranges"],
            json!([{ "start_line": 4, "end_line": 4 }])
        );
        assert_eq!((totals.commits, totals.added, totals.attributed), (2, 5, 2));
        // Stable ids, valid UUIDs, and the spec's required fields.
        assert_eq!(
            recs[0]["id"],
            record_id(recs[0]["vcs"]["revision"].as_str().unwrap())
        );
        assert_eq!(recs[0]["id"].as_str().unwrap().len(), 36);
        for k in ["version", "id", "timestamp", "files"] {
            assert!(recs[0].get(k).is_some(), "{k}");
        }
        assert_eq!(recs[0]["timestamp"], "2027-01-15T08:00:00Z");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn short_lines_join_the_range_of_the_session_around_them() {
        let w = |s: &str| Written {
            session: s.into(),
            ts: 1,
            model: None,
        };
        let written: HashMap<String, Written> = [
            ("let first_agent_line = 1;".to_string(), w("a")),
            ("let second_agent_line = 2;".to_string(), w("a")),
            ("let other_session_line = 3;".to_string(), w("b")),
        ]
        .into_iter()
        .collect();
        let added: Vec<(u64, String)> = [
            "let first_agent_line = 1;",
            "}",
            "let second_agent_line = 2;",
            "",
            "let other_session_line = 3;",
        ]
        .iter()
        .enumerate()
        .map(|(i, t)| (i as u64 + 1, t.to_string()))
        .collect();
        let who: Vec<Option<String>> = attribute(&added, &written)
            .into_iter()
            .map(|w| w.map(|w| w.session))
            .collect();
        assert_eq!(
            who,
            vec![
                Some("a".into()),
                Some("a".into()),
                Some("a".into()),
                None,
                Some("b".into())
            ]
        );
    }

    #[test]
    fn model_names_follow_models_dev() {
        assert_eq!(model_id("claude-opus-5-5"), "anthropic/claude-opus-5-5");
        assert_eq!(
            model_id("developer/claude-opus-5-5"),
            "anthropic/claude-opus-5-5"
        );
        assert_eq!(model_id("gpt-5.6-sol"), "openai/gpt-5.6-sol");
        assert_eq!(model_id("o3-mini"), "openai/o3-mini");
        assert_eq!(model_id("local-model"), "local-model");
    }
}
