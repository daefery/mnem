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

/// The model a build uses when the settings name none: MiniLM where the build can run it
/// (picked on judged real prompts, see docs/reference.md), else potion-base-8M.
#[cfg(feature = "fastembed")]
pub const DEFAULT_MODEL: &str = "fastembed:AllMiniLML6V2";
#[cfg(not(feature = "fastembed"))]
pub const DEFAULT_MODEL: &str = "minishlab/potion-base-8M";
const FILES: &[&str] = &["tokenizer.json", "model.safetensors", "config.json"];

/// The configured model, under one canonical name per model (thresholds and vector
/// keys depend on it): fastembed models by their enum name, whatever the config says.
pub fn model_name() -> String {
    let name = crate::config::CONFIG
        .semantic
        .model
        .clone()
        .unwrap_or_else(|| DEFAULT_MODEL.to_string());
    #[cfg(feature = "fastembed")]
    if let Some(m) = name.strip_prefix("fastembed:")
        && let Some(info) = fastembed_info(m)
    {
        return format!("fastembed:{:?}", info.model);
    }
    name
}

/// A model fastembed lists, by its Hugging Face code or enum name.
#[cfg(feature = "fastembed")]
fn fastembed_info(m: &str) -> Option<fastembed::ModelInfo<fastembed::EmbeddingModel>> {
    fastembed::TextEmbedding::list_supported_models()
        .into_iter()
        .find(|i| {
            i.model_code.eq_ignore_ascii_case(m) || format!("{:?}", i.model).eq_ignore_ascii_case(m)
        })
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
    Onnx(Box<std::sync::Mutex<fastembed::TextEmbedding>>),
}

pub struct Embedder {
    backend: Backend,
    /// Text some models expect before a query and before a document.
    prefixes: (&'static str, &'static str),
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
            let info = fastembed_info(m).with_context(|| format!("unknown fastembed model {m}"))?;
            let opts = fastembed::TextInitOptions::new(info.model.clone())
                .with_cache_dir(db::data_dir().join("models").join("fastembed"))
                .with_max_length(MAX_TOKENS)
                .with_show_download_progress(false);
            let mut te = fastembed::TextEmbedding::try_new(opts)?;
            // Key vectors by what produced them: every file the loader reads (resolved as
            // it resolves them), the input length and the prefixes, plus the model's
            // output for fixed probes (one longer than the input limit) as a second check.
            let prefixes = prefixes(m);
            let long = "memory fingerprint probe sentence ".repeat(80);
            let probes: Vec<String> = [
                "mnem fingerprint",
                "why does the backup restore hang",
                &long,
            ]
            .iter()
            .map(|p| format!("{}{p}", prefixes.1))
            .collect();
            let out = te.embed(&probes, None)?;
            if out.iter().any(|v| v.is_empty()) {
                bail!("fastembed model {m} returned empty embeddings");
            }
            let mut h = Sha256::new();
            for (file, path) in onnx_assets(&info)? {
                h.update(file.as_bytes());
                h.update(std::fs::read(&path).with_context(|| format!("read {}", path.display()))?);
            }
            h.update(format!("{MAX_TOKENS}|{}|{}|", prefixes.0, prefixes.1).as_bytes());
            for v in &out {
                for x in v {
                    h.update(((x * 100.0).round() as i32).to_le_bytes());
                }
            }
            return Ok(Embedder {
                backend: Backend::Onnx(Box::new(std::sync::Mutex::new(te))),
                key: format!("{name}@{}", hex(&h.finalize()[..8])),
                prefixes,
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
            prefixes: ("", ""),
            key: format!("{name}@{}", hex(&h.finalize()[..8])),
            name,
        })
    }

    /// Embed documents (memories).
    pub fn embed(&self, texts: &[String]) -> Vec<Vec<f32>> {
        if self.prefixes.1.is_empty() {
            return self.embed_raw(texts);
        }
        let texts: Vec<String> = texts
            .iter()
            .map(|t| format!("{}{t}", self.prefixes.1))
            .collect();
        self.embed_raw(&texts)
    }

    fn embed_raw(&self, texts: &[String]) -> Vec<Vec<f32>> {
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

/// Input length for ONNX models, in tokens (memories are embedded from their head).
#[cfg(feature = "fastembed")]
const MAX_TOKENS: usize = 256;

/// The files fastembed loads for a model, found the way its Hugging Face cache finds
/// them: HF_HOME if set, else mnem's model cache; `refs/main` names the snapshot.
/// Any missing file is an error: an incomplete fingerprint would not tell models apart.
#[cfg(feature = "fastembed")]
fn onnx_assets(
    info: &fastembed::ModelInfo<fastembed::EmbeddingModel>,
) -> Result<Vec<(String, PathBuf)>> {
    let root = std::env::var("HF_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| db::data_dir().join("models").join("fastembed"));
    let repo = root.join(format!("models--{}", info.model_code.replace('/', "--")));
    let rev = std::fs::read_to_string(repo.join("refs").join("main"))
        .with_context(|| format!("no snapshot of {} in {}", info.model_code, root.display()))?;
    let snap = repo.join("snapshots").join(rev.trim());
    let mut files = vec![info.model_file.clone()];
    files.extend(info.additional_files.iter().cloned());
    for f in [
        "tokenizer.json",
        "config.json",
        "special_tokens_map.json",
        "tokenizer_config.json",
    ] {
        files.push(f.to_string());
    }
    files
        .into_iter()
        .map(|f| {
            let p = snap.join(&f);
            if p.is_file() {
                Ok((f, p))
            } else {
                bail!("model file {} missing from {}", f, snap.display())
            }
        })
        .collect()
}

/// Query and document prefixes the model was trained with.
#[cfg(feature = "fastembed")]
fn prefixes(model: &str) -> (&'static str, &'static str) {
    let m = model.to_lowercase();
    if m.contains("e5") {
        ("query: ", "passage: ")
    } else if m.contains("nomic") {
        ("search_query: ", "search_document: ")
    } else if m.contains("arctic") || m.contains("bge") && m.contains("en") {
        (
            "Represent this sentence for searching relevant passages: ",
            "",
        )
    } else {
        ("", "")
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

/// Copy this model's vectors from another mnem database (for example one where a model
/// was tried out), keeping only those whose memory text here still hashes the same.
/// Returns (copied, skipped).
pub fn import(
    conn: &mut Connection,
    key: &str,
    dim: usize,
    from: &std::path::Path,
) -> Result<(usize, usize)> {
    let src = Connection::open_with_flags(from, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut st = src.prepare(
        "SELECT memory_id, dim, scale, vec, text_hash FROM memory_vectors WHERE model = ?1 AND text_hash IS NOT NULL",
    )?;
    let rows: Vec<(i64, i64, f32, Vec<u8>, String)> = st
        .query_map([key], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let (mut copied, mut skipped) = (0, 0);
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    {
        let mut cur = tx.prepare_cached(&format!(
            "SELECT {MEMORY_TEXT_COLS} FROM memories m WHERE m.id = ?1 AND m.kind != 'pinned'"
        ))?;
        let mut ins = tx.prepare_cached(
            "INSERT OR REPLACE INTO memory_vectors(memory_id, model, dim, scale, vec, text_hash)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?;
        for (id, stored_dim, scale, vec, hash) in rows {
            // Only well-formed vectors of this model's size, for text that still matches.
            let sound =
                stored_dim == dim as i64 && vec.len() == dim && scale.is_finite() && scale > 0.0;
            let now: Option<String> = cur.query_row([id], |r| text_from_row(r, 0)).optional()?;
            if sound && now.as_deref().map(text_hash).as_deref() == Some(hash.as_str()) {
                ins.execute(params![id, key, stored_dim, scale, vec, hash])?;
                copied += 1;
            } else {
                skipped += 1;
            }
        }
    }
    tx.commit()?;
    Ok((copied, skipped))
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
            vec: self
                .embed_raw(&[format!("{}{text}", self.prefixes.0)])
                .pop()
                .unwrap_or_default(),
        }
    }
}

/// The embedding model, loaded once per process and shared (the watch service uses it
/// for its HTTP endpoint and background embedding). Loading runs on its own thread, so a
/// slow or stalled model download never holds up the caller: until it is ready this
/// returns None (hooks fall back to keywords). A failed load is retried at most once a
/// minute, so downloading the model later needs no restart.
pub fn shared() -> Option<std::sync::Arc<Embedder>> {
    use std::sync::{Arc, Mutex};
    enum State {
        Idle(Option<std::time::Instant>),
        Loading,
        Ready(Arc<Embedder>),
    }
    static STATE: Mutex<State> = Mutex::new(State::Idle(None));
    let mut st = STATE.lock().ok()?;
    match &*st {
        State::Ready(e) => return Some(e.clone()),
        State::Loading => return None,
        State::Idle(Some(t)) if t.elapsed() < Duration::from_secs(60) => return None,
        State::Idle(_) => {}
    }
    *st = State::Loading;
    std::thread::spawn(|| {
        // The service fetches a missing static model once (for example after moving to a
        // new machine); hooks never download anything.
        let name = model_name();
        if crate::recall::semantic_enabled() && !name.starts_with("fastembed:") {
            let _ = fetch(&name);
        }
        let loaded = crate::recall::semantic_embedder().map(Arc::new);
        if let Ok(mut st) = STATE.lock() {
            *st = match loaded {
                Some(e) => State::Ready(e),
                None => State::Idle(Some(std::time::Instant::now())),
            };
        }
    });
    None
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

/// Memory ids in `project` by cosine similarity to the query, best first, within
/// `scope`. Pinned and sensitive memories and those outside the scope are filtered in
/// SQL so they can never crowd eligible memories out of the top `limit`.
pub fn search(
    conn: &Connection,
    q: &Query,
    project: &str,
    scope: &crate::recall::Scope,
    limit: usize,
) -> Result<Vec<(i64, f32)>> {
    search_where(
        conn,
        q,
        &format!(
            "m.project = ? AND m.kind != 'pinned' AND coalesce(m.type, '') != 'sensitive'
             AND NOT EXISTS (SELECT 1 FROM recall_seen r WHERE r.session_id = ? AND r.memory_id = m.id)
             AND (? = '' OR coalesce(m.session_id, '') != ?) AND coalesce(m.created_at, 0) < ?
             AND {}",
            crate::scripted::MEMORY_NOT_SCRIPTED
        ),
        vec![
            Box::new(project.to_string()),
            Box::new(scope.offered_to.unwrap_or("").to_string()),
            Box::new(scope.session.unwrap_or("").to_string()),
            Box::new(scope.session.unwrap_or("").to_string()),
            Box::new(scope.before.unwrap_or(i64::MAX)),
        ],
        limit,
    )
}

/// Cosine similarity of the query to each of `ids` that has a vector for its model.
pub fn cosines(
    conn: &Connection,
    q: &Query,
    ids: &[i64],
) -> Result<std::collections::HashMap<i64, f32>> {
    let mut st = conn.prepare_cached(
        "SELECT scale, vec FROM memory_vectors WHERE memory_id = ?1 AND model = ?2",
    )?;
    let mut out = std::collections::HashMap::new();
    for id in ids {
        let row: Option<(f32, Vec<u8>)> = st
            .query_row(params![id, q.model], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        if let Some((scale, v)) = row.filter(|(_, v)| v.len() == q.vec.len()) {
            out.insert(*id, dot_q(&q.vec, scale, &v));
        }
    }
    Ok(out)
}

/// Memory ids by cosine similarity to the query, best first, among memories `m`
/// matching the SQL condition `filter` (with `?` placeholders bound to `args`).
pub fn search_where(
    conn: &Connection,
    q: &Query,
    filter: &str,
    args: Vec<Box<dyn rusqlite::ToSql>>,
    limit: usize,
) -> Result<Vec<(i64, f32)>> {
    let mut st = conn.prepare_cached(&format!(
        "SELECT v.memory_id, v.scale, v.vec FROM memory_vectors v JOIN memories m ON m.id = v.memory_id
         WHERE v.model = ? AND ({filter})"
    ))?;
    let mut all: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(q.model.clone())];
    all.extend(args);
    let mut scored: Vec<(i64, f32)> = st
        .query_map(
            rusqlite::params_from_iter(all.iter().map(|b| b.as_ref())),
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

    #[test]
    fn import_skips_vectors_of_changed_text() {
        let dir = std::env::temp_dir().join(format!("mnem-import-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let open = |name: &str| {
            let p = dir.join(name);
            let _ = std::fs::remove_file(&p);
            (db::open_with(&p, Duration::from_secs(1)).unwrap(), p)
        };
        let seed = |c: &Connection| {
            for id in [1, 2, 3] {
                c.execute(
                    "INSERT INTO memories(id, project, kind, title, narrative, origin, origin_id, created_at) VALUES (?1, 'p', 'observation', ?2, 'n', 'mnem', ?1, 0)",
                    params![id, format!("title {id}")],
                )
                .unwrap();
            }
        };
        let (mut src, src_path) = open("src.db");
        seed(&src);
        let read: Vec<(i64, String)> = [1, 2, 3]
            .iter()
            .map(|id| (*id, memory_text(&format!("title {id}"), "", "n", "[]")))
            .collect();
        assert_eq!(
            store(&mut src, "m", &read, vec![vec![0.5; 4]; 3]).unwrap(),
            3
        );
        drop(src);
        let (mut dst, _) = open("dst.db");
        seed(&dst);
        dst.execute("UPDATE memories SET title = 'edited' WHERE id = 2", [])
            .unwrap();
        dst.execute("DELETE FROM memories WHERE id = 3", [])
            .unwrap();
        assert_eq!(import(&mut dst, "m", 4, &src_path).unwrap(), (1, 2));
        let count = |c: &Connection| -> i64 {
            c.query_row(
                "SELECT count(*) FROM memory_vectors WHERE memory_id = 1",
                [],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(count(&dst), 1);
        // Malformed vectors are never copied, even for unchanged text.
        let bad = Connection::open(&src_path).unwrap();
        bad.execute(
            "UPDATE memory_vectors SET scale = -1.0 WHERE memory_id = 1",
            [],
        )
        .unwrap();
        drop(bad);
        dst.execute("DELETE FROM memory_vectors", []).unwrap();
        assert_eq!(import(&mut dst, "m", 4, &src_path).unwrap(), (0, 3));
        assert_eq!(count(&dst), 0);
        // Nor vectors of another size than the model's.
        assert_eq!(import(&mut dst, "m", 8, &src_path).unwrap(), (0, 3));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
