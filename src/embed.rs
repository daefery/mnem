//! Local text embeddings for semantic recall (Model2Vec static embeddings).
//!
//! Static embeddings are a token lookup plus mean pooling: no neural network runs at
//! query time, so a query embeds in well under a millisecond on the CPU, and nothing
//! leaves the machine. Vectors are stored in SQLite as int8 with a per-vector scale.

use crate::db;
use anyhow::{Context, Result, bail};
use model2vec_rs::model::StaticModel;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::time::Duration;

pub const DEFAULT_MODEL: &str = "minishlab/potion-base-8M";
const FILES: &[&str] = &["tokenizer.json", "model.safetensors", "config.json"];

pub fn model_name() -> String {
    crate::config::CONFIG
        .semantic
        .model
        .clone()
        .unwrap_or_else(|| DEFAULT_MODEL.to_string())
}

fn model_dir(name: &str) -> PathBuf {
    db::data_dir().join("models").join(name.replace('/', "--"))
}

/// Download the model's three files from Hugging Face into ~/.mnem/models once.
/// ONNX models (fastembed:...) download themselves when first loaded.
pub fn fetch(name: &str) -> Result<PathBuf> {
    if name.starts_with("fastembed:") {
        return Ok(db::data_dir().join("models").join("fastembed"));
    }
    let dir = model_dir(name);
    if FILES.iter().all(|f| dir.join(f).exists())
        && StaticModel::from_pretrained(&dir, None, Some(true), None).is_ok()
    {
        return Ok(dir);
    }
    std::fs::create_dir_all(&dir)?;
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(300)))
        .build()
        .new_agent();
    for f in FILES {
        let url = format!("https://huggingface.co/{name}/resolve/main/{f}");
        let mut resp = agent
            .get(&url)
            .call()
            .with_context(|| format!("download {url}"))?;
        let bytes = resp.body_mut().with_config().limit(1 << 30).read_to_vec()?;
        let tmp = dir.join(format!("{f}.partial"));
        std::fs::write(&tmp, &bytes)?;
        std::fs::rename(&tmp, dir.join(f))?;
    }
    // Existence is not integrity: a truncated file would otherwise count as done forever.
    if let Err(e) = StaticModel::from_pretrained(&dir, None, Some(true), None) {
        for f in FILES {
            let _ = std::fs::remove_file(dir.join(f));
        }
        bail!("downloaded model {name} does not load ({e:#}); removed it, run `mnem embed` again");
    }
    Ok(dir)
}

enum Backend {
    Static(StaticModel),
    /// Transformer model through ONNX Runtime (build with `--features fastembed`).
    #[cfg(feature = "fastembed")]
    Onnx(std::sync::Mutex<fastembed::TextEmbedding>),
}

pub struct Embedder {
    backend: Backend,
    pub name: String,
    /// What vectors and queries are keyed by: the model name plus a fingerprint of its
    /// files, so vectors from another download of the same name are never compared.
    pub key: String,
}

impl Embedder {
    /// Load the configured model if it has been downloaded (never downloads a static
    /// model; ONNX models are fetched into ~/.mnem/models on first use).
    pub fn load() -> Result<Embedder> {
        let name = model_name();
        #[cfg(feature = "fastembed")]
        if let Some(m) = name.strip_prefix("fastembed:") {
            let model = match m {
                "bge-small-en-v1.5" => fastembed::EmbeddingModel::BGESmallENV15,
                "bge-small-en-v1.5-q" => fastembed::EmbeddingModel::BGESmallENV15Q,
                "all-MiniLM-L6-v2" => fastembed::EmbeddingModel::AllMiniLML6V2,
                other => bail!("unknown fastembed model {other}"),
            };
            let opts = fastembed::TextInitOptions::new(model)
                .with_cache_dir(db::data_dir().join("models").join("fastembed"))
                .with_show_download_progress(false);
            let te = fastembed::TextEmbedding::try_new(opts)?;
            return Ok(Embedder {
                backend: Backend::Onnx(std::sync::Mutex::new(te)),
                key: format!("{name}@fastembed-{}", env!("CARGO_PKG_VERSION")),
                name,
            });
        }
        let dir = model_dir(&name);
        if !FILES.iter().all(|f| dir.join(f).exists()) {
            bail!("embedding model {name} not downloaded (run `mnem embed`)");
        }
        let model = StaticModel::from_pretrained(&dir, None, Some(true), None)?;
        let mut h = Sha256::new();
        for f in FILES {
            h.update(std::fs::read(dir.join(f))?);
        }
        Ok(Embedder {
            backend: Backend::Static(model),
            key: format!("{name}@{}", hex(&h.finalize()[..8])),
            name,
        })
    }

    pub fn embed(&self, texts: &[String]) -> Vec<Vec<f32>> {
        match &self.backend {
            Backend::Static(m) => m.encode_with_args(texts, Some(256), 512),
            #[cfg(feature = "fastembed")]
            Backend::Onnx(m) => m
                .lock()
                .map(|mut m| m.embed(texts, Some(64)).unwrap_or_default())
                .unwrap_or_default(),
        }
    }
}

/// int8 quantisation with one scale per vector: 4x smaller, cosine barely changes.
pub fn quantize(v: &[f32]) -> (f32, Vec<u8>) {
    let max = v.iter().fold(0f32, |m, x| m.max(x.abs())).max(1e-12);
    let scale = max / 127.0;
    (
        scale,
        v.iter()
            .map(|x| ((x / scale).round().clamp(-127.0, 127.0) as i8) as u8)
            .collect(),
    )
}

pub fn dot_q(query: &[f32], scale: f32, q: &[u8]) -> f32 {
    query
        .iter()
        .zip(q)
        .map(|(a, b)| a * f32::from(*b as i8))
        .sum::<f32>()
        * scale
}

/// The text a memory is embedded from.
pub fn memory_text(title: &str, subtitle: &str, narrative: &str, facts: &str) -> String {
    let facts: Vec<String> = serde_json::from_str(facts).unwrap_or_default();
    let mut t = format!(
        "{title}. {subtitle}. {}",
        crate::text::head(narrative, 1200)
    );
    if !facts.is_empty() {
        t.push(' ');
        t.push_str(&facts.join(" "));
    }
    t
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Identity of the text a vector was computed from.
pub fn text_hash(text: &str) -> String {
    hex(&Sha256::digest(text.as_bytes())[..8])
}

const MEMORY_TEXT_COLS: &str = "coalesce(m.title, ''), coalesce(m.subtitle, ''), coalesce(m.narrative, ''), coalesce(m.facts, '[]')";

fn text_from_row(r: &rusqlite::Row, at: usize) -> rusqlite::Result<String> {
    Ok(memory_text(
        &r.get::<_, String>(at)?,
        &r.get::<_, String>(at + 1)?,
        &r.get::<_, String>(at + 2)?,
        &r.get::<_, String>(at + 3)?,
    ))
}

/// Embed memories that have no current vector for this model (none yet, or one from
/// before text hashes were stored). Returns how many were written.
///
/// Embedding happens outside any transaction, so a memory can change or disappear
/// between reading its text and writing its vector. Each write therefore re-reads the
/// memory inside the write transaction and is skipped unless the text still hashes the
/// same; the next pass embeds the new text.
pub fn backfill(conn: &mut Connection, e: &Embedder, limit: Option<usize>) -> Result<usize> {
    let rows: Vec<(i64, String)> = {
        let mut st = conn.prepare(&format!(
            "SELECT m.id, {MEMORY_TEXT_COLS}
             FROM memories m LEFT JOIN memory_vectors v ON v.memory_id = m.id AND v.model = ?1
             WHERE (v.memory_id IS NULL OR v.text_hash IS NULL) AND m.kind != 'pinned' LIMIT ?2"
        ))?;
        st.query_map(params![e.key, limit.map(|l| l as i64).unwrap_or(-1)], |r| {
            Ok((r.get(0)?, text_from_row(r, 1)?))
        })?
        .collect::<rusqlite::Result<_>>()?
    };
    let mut done = 0;
    for chunk in rows.chunks(2048) {
        let texts: Vec<String> = chunk.iter().map(|(_, t)| t.clone()).collect();
        let vecs = e.embed(&texts);
        done += store(conn, &e.key, chunk, vecs)?;
    }
    Ok(done)
}

/// Delete vectors of every other model or model revision. Only `mnem embed` calls this:
/// the long-running service never deletes, so a process still holding an older model
/// cannot remove vectors a newer one wrote.
pub fn prune(conn: &Connection, e: &Embedder) -> Result<usize> {
    Ok(conn.execute("DELETE FROM memory_vectors WHERE model != ?1", [&e.key])?)
}

/// Write vectors for `(memory id, embedded text)` pairs, skipping any memory whose text
/// no longer matches (changed or deleted since it was read).
fn store(
    conn: &mut Connection,
    model: &str,
    chunk: &[(i64, String)],
    vecs: Vec<Vec<f32>>,
) -> Result<usize> {
    let mut done = 0;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    {
        let mut cur = tx.prepare_cached(&format!(
            "SELECT {MEMORY_TEXT_COLS} FROM memories m WHERE m.id = ?1 AND m.kind != 'pinned'"
        ))?;
        let mut ins = tx.prepare_cached(
            "INSERT OR REPLACE INTO memory_vectors(memory_id, model, dim, scale, vec, text_hash)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?;
        for ((id, text), v) in chunk.iter().zip(vecs) {
            let hash = text_hash(text);
            let now: Option<String> = cur.query_row([id], |r| text_from_row(r, 0)).optional()?;
            if v.is_empty() || now.as_deref().map(text_hash).as_deref() != Some(hash.as_str()) {
                continue;
            }
            let (scale, q) = quantize(&v);
            ins.execute(params![id, model, q.len() as i64, scale, q, hash])?;
            done += 1;
        }
    }
    tx.commit()?;
    Ok(done)
}

/// A query vector and the model key (name and revision) that produced it: vectors from
/// different models or revisions are not comparable.
pub struct Query {
    pub model: String,
    pub vec: Vec<f32>,
}

impl Embedder {
    pub fn query(&self, text: &str) -> Query {
        Query {
            model: self.key.clone(),
            vec: self.embed(&[text.to_string()]).pop().unwrap_or_default(),
        }
    }
}

/// The embedding model, loaded once per process and shared (the watch service uses it
/// for its HTTP endpoint and background embedding). A failed load is retried at most
/// once a minute, so downloading the model later needs no restart.
pub fn shared() -> Option<std::sync::Arc<Embedder>> {
    use std::sync::{Arc, Mutex};
    static STATE: Mutex<(Option<Arc<Embedder>>, Option<std::time::Instant>)> =
        Mutex::new((None, None));
    let mut st = STATE.lock().ok()?;
    if let Some(e) = &st.0 {
        return Some(e.clone());
    }
    if st.1.is_some_and(|t| t.elapsed() < Duration::from_secs(60)) {
        return None;
    }
    st.1 = Some(std::time::Instant::now());
    st.0 = crate::recall::semantic_embedder().map(Arc::new);
    st.0.clone()
}

/// Port `mnem watch` serves the viewer and embeddings on unless told otherwise:
/// MNEM_UI_PORT, then config `ui_port`, then 37777.
pub fn configured_port() -> u16 {
    std::env::var("MNEM_UI_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .or(crate::config::CONFIG.ui_port)
        .unwrap_or(37777)
}

const PORT_KEY: &str = "watch.ui_port";

/// Record the port this watch process listens on (0: none), with its pid, so hooks find
/// it even when it came from `mnem watch --ui-port`.
pub fn record_service_port(conn: &Connection, port: u16) -> Result<()> {
    conn.execute(
        "INSERT INTO meta(k, v) VALUES (?1, ?2) ON CONFLICT(k) DO UPDATE SET v = excluded.v",
        params![PORT_KEY, format!("{port} {}", std::process::id())],
    )?;
    Ok(())
}

/// Port of the running watch service, None when there is none. MNEM_UI_PORT wins; then
/// the port the service recorded, as long as the process that recorded it is alive (a
/// stale record must not send prompts to whatever took the port later); without any
/// record, the configured default.
pub fn service_port(conn: &Connection) -> Option<u16> {
    if let Some(p) = std::env::var("MNEM_UI_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
    {
        return Some(p);
    }
    let rec: Option<String> = conn
        .query_row("SELECT v FROM meta WHERE k = ?1", [PORT_KEY], |r| r.get(0))
        .ok();
    let port = match rec {
        None => configured_port(),
        Some(v) => {
            let mut it = v.split_whitespace();
            let port: u16 = it.next()?.parse().ok()?;
            let pid: u32 = it.next()?.parse().ok()?;
            if !process_alive(pid) {
                return None;
            }
            port
        }
    };
    (port != 0).then_some(port)
}

fn process_alive(pid: u32) -> bool {
    let proc = std::path::Path::new("/proc");
    // Without procfs (macOS) the record is trusted as is.
    !proc.is_dir() || proc.join(pid.to_string()).exists()
}

/// Embed `text` in the watch service, where the model stays loaded. None when the
/// service is disabled or not reachable within the timeout (callers fall back to keywords).
pub fn query_from_service(conn: &Connection, text: &str) -> Option<Query> {
    let port = service_port(conn)?;
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_millis(300)))
        .build()
        .new_agent();
    let url = format!("http://127.0.0.1:{port}/api/embed");
    let mut r = agent.get(&url).query("q", text).call().ok()?;
    let v: serde_json::Value = r.body_mut().read_json().ok()?;
    Some(Query {
        model: v["model"].as_str()?.to_string(),
        vec: v["vector"]
            .as_array()?
            .iter()
            .filter_map(|x| x.as_f64().map(|f| f as f32))
            .collect(),
    })
}

/// Memory ids in `project` by cosine similarity to the query, best first. Pinned and
/// sensitive memories, and ones already offered to `exclude_session`, are filtered in
/// SQL so they can never crowd eligible memories out of the top `limit`.
pub fn search(
    conn: &Connection,
    q: &Query,
    project: &str,
    exclude_session: Option<&str>,
    limit: usize,
) -> Result<Vec<(i64, f32)>> {
    let mut st = conn.prepare_cached(
        "SELECT v.memory_id, v.scale, v.vec FROM memory_vectors v JOIN memories m ON m.id = v.memory_id
         WHERE v.model = ?1 AND m.project = ?2 AND m.kind != 'pinned' AND coalesce(m.type, '') != 'sensitive'
           AND NOT EXISTS (SELECT 1 FROM recall_seen r WHERE r.session_id = ?3 AND r.memory_id = m.id)",
    )?;
    let mut scored: Vec<(i64, f32)> = st
        .query_map(
            params![q.model, project, exclude_session.unwrap_or("")],
            |r| {
                let (id, scale, v): (i64, f32, Vec<u8>) = (r.get(0)?, r.get(1)?, r.get(2)?);
                Ok((
                    id,
                    if v.len() == q.vec.len() {
                        dot_q(&q.vec, scale, &v)
                    } else {
                        -1.0
                    },
                ))
            },
        )?
        .collect::<rusqlite::Result<_>>()?;
    // Partial selection: only the top `limit` need ordering.
    if scored.len() > limit {
        scored.select_nth_unstable_by(limit, |a, b| b.1.total_cmp(&a.1));
        scored.truncate(limit);
    }
    scored.sort_by(|a, b| b.1.total_cmp(&a.1));
    Ok(scored)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantization_keeps_cosine() {
        let a: Vec<f32> = (0..256)
            .map(|i| ((i * 37 % 101) as f32 - 50.0) / 50.0)
            .collect();
        let n = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        let a: Vec<f32> = a.iter().map(|x| x / n).collect();
        let (scale, q) = quantize(&a);
        let exact: f32 = a.iter().map(|x| x * x).sum();
        assert!((dot_q(&a, scale, &q) - exact).abs() < 0.01);
    }

    #[test]
    fn stale_text_is_not_stored() {
        let mut conn =
            db::open_with(std::path::Path::new(":memory:"), Duration::from_secs(1)).unwrap();
        for id in [1, 2, 3] {
            conn.execute(
                "INSERT INTO memories(id, project, kind, title, narrative, origin, origin_id, created_at) VALUES (?1, 'p', 'observation', ?2, 'n', 'mnem', ?1, 0)",
                params![id, format!("title {id}")],
            )
            .unwrap();
        }
        let read: Vec<(i64, String)> = [1, 2, 3]
            .iter()
            .map(|id| (*id, memory_text(&format!("title {id}"), "", "n", "[]")))
            .collect();
        // Between reading and storing: #2 is edited, #3 is deleted.
        conn.execute("UPDATE memories SET title = 'edited' WHERE id = 2", [])
            .unwrap();
        conn.execute("DELETE FROM memories WHERE id = 3", [])
            .unwrap();
        let n = store(&mut conn, "m", &read, vec![vec![0.5; 4]; 3]).unwrap();
        assert_eq!(n, 1);
        let ids: Vec<i64> = conn
            .prepare("SELECT memory_id FROM memory_vectors")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(ids, vec![1]);
    }

    #[test]
    fn service_port_follows_live_watch() {
        if std::env::var("MNEM_UI_PORT").is_ok() {
            return;
        }
        let conn = db::open_with(std::path::Path::new(":memory:"), Duration::from_secs(1)).unwrap();
        record_service_port(&conn, 40123).unwrap();
        assert_eq!(service_port(&conn), Some(40123));
        record_service_port(&conn, 0).unwrap();
        assert_eq!(service_port(&conn), None);
        if std::path::Path::new("/proc").is_dir() {
            conn.execute(
                "UPDATE meta SET v = '40123 4294967295' WHERE k = ?1",
                [PORT_KEY],
            )
            .unwrap();
            assert_eq!(service_port(&conn), None, "stale record from a dead watch");
        }
    }
}
