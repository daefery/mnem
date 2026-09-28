//! Local text embeddings for semantic recall (Model2Vec static embeddings).
//!
//! Static embeddings are a token lookup plus mean pooling: no neural network runs at
//! query time, so a query embeds in well under a millisecond on the CPU, and nothing
//! leaves the machine. Vectors are stored in SQLite as int8 with a per-vector scale.

use crate::db;
use anyhow::{Context, Result, bail};
use model2vec_rs::model::StaticModel;
use rusqlite::{Connection, params};
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
    if FILES.iter().all(|f| dir.join(f).exists()) {
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
                name,
            });
        }
        let dir = model_dir(&name);
        if !FILES.iter().all(|f| dir.join(f).exists()) {
            bail!("embedding model {name} not downloaded (run `mnem embed`)");
        }
        let model = StaticModel::from_pretrained(&dir, None, Some(true), None)?;
        Ok(Embedder {
            backend: Backend::Static(model),
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

/// Embed memories that have no vector for the current model. Returns how many.
pub fn backfill(conn: &mut Connection, e: &Embedder, limit: Option<usize>) -> Result<usize> {
    let rows: Vec<(i64, String)> = {
        let mut st = conn.prepare(
            "SELECT m.id, coalesce(m.title, ''), coalesce(m.subtitle, ''), coalesce(m.narrative, ''), coalesce(m.facts, '[]')
             FROM memories m LEFT JOIN memory_vectors v ON v.memory_id = m.id AND v.model = ?1
             WHERE v.memory_id IS NULL AND m.kind != 'pinned' LIMIT ?2",
        )?;
        st.query_map(
            params![e.name, limit.map(|l| l as i64).unwrap_or(-1)],
            |r| {
                Ok((
                    r.get(0)?,
                    memory_text(
                        &r.get::<_, String>(1)?,
                        &r.get::<_, String>(2)?,
                        &r.get::<_, String>(3)?,
                        &r.get::<_, String>(4)?,
                    ),
                ))
            },
        )?
        .collect::<rusqlite::Result<_>>()?
    };
    let mut done = 0;
    for chunk in rows.chunks(2048) {
        let texts: Vec<String> = chunk.iter().map(|(_, t)| t.clone()).collect();
        let vecs = e.embed(&texts);
        let tx = conn.transaction()?;
        {
            let mut ins = tx.prepare_cached(
                "INSERT OR REPLACE INTO memory_vectors(memory_id, model, dim, scale, vec) VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for ((id, _), v) in chunk.iter().zip(vecs) {
                let (scale, q) = quantize(&v);
                ins.execute(params![id, e.name, q.len() as i64, scale, q])?;
            }
        }
        tx.commit()?;
        done += chunk.len();
    }
    Ok(done)
}

/// A query vector and the model that produced it (vectors from different models are
/// not comparable).
pub struct Query {
    pub model: String,
    pub vec: Vec<f32>,
}

impl Embedder {
    pub fn query(&self, text: &str) -> Query {
        Query {
            model: self.name.clone(),
            vec: self.embed(&[text.to_string()]).pop().unwrap_or_default(),
        }
    }
}

/// The embedding model, loaded once per process on first use (the watch service shares
/// it between its HTTP endpoint and background embedding).
pub fn shared() -> Option<&'static Embedder> {
    static E: std::sync::OnceLock<Option<Embedder>> = std::sync::OnceLock::new();
    E.get_or_init(crate::recall::semantic_embedder).as_ref()
}

/// Port of the local viewer/embedding service run by `mnem watch`.
pub fn service_port() -> u16 {
    std::env::var("MNEM_UI_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(37777)
}

/// Embed `text` in the watch service, where the model stays loaded. None when the
/// service is not reachable within the timeout (callers fall back to keywords).
pub fn query_from_service(text: &str) -> Option<Query> {
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_millis(300)))
        .build()
        .new_agent();
    let url = format!("http://127.0.0.1:{}/api/embed", service_port());
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

/// Memory ids in `project` by cosine similarity to the query, best first.
pub fn search(
    conn: &Connection,
    q: &Query,
    project: &str,
    limit: usize,
) -> Result<Vec<(i64, f32)>> {
    let mut st = conn.prepare_cached(
        "SELECT v.memory_id, v.scale, v.vec FROM memory_vectors v JOIN memories m ON m.id = v.memory_id
         WHERE v.model = ?1 AND m.project = ?2",
    )?;
    let mut scored: Vec<(i64, f32)> = st
        .query_map(params![q.model, project], |r| {
            let (id, scale, v): (i64, f32, Vec<u8>) = (r.get(0)?, r.get(1)?, r.get(2)?);
            Ok((
                id,
                if v.len() == q.vec.len() {
                    dot_q(&q.vec, scale, &v)
                } else {
                    -1.0
                },
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    scored.sort_by(|a, b| b.1.total_cmp(&a.1));
    scored.truncate(limit);
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
}
