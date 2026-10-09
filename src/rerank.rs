//! Cross-encoder reranking, for evaluation: the model reads the prompt and a memory
//! together and scores how well the memory answers it. `rvn eval --rerank <model>
//! --dump` records its scores beside the embedding cosine so the two can be compared
//! with `--analyze file:rerank file:cos`.
//!
//! Not used by recall. Screened on judged real prompts (28 Sep 2026), no reranker was
//! clearly better than the MiniLM cosine gate (AUC jina-v1-turbo 0.728, bge-base 0.702,
//! jina-v2-multilingual 0.745 against 0.713, all within noise) and each took 0.9-3.4 s
//! per prompt on this CPU, against a hook budget of about 300 ms.

#[cfg(feature = "fastembed")]
use anyhow::Context;
use anyhow::Result;

pub struct Reranker {
    #[cfg(feature = "fastembed")]
    inner: std::sync::Mutex<fastembed::TextRerank>,
    pub name: String,
}

impl Reranker {
    /// Load a fastembed reranker by its enum name (e.g. JINARerankerV1TurboEn).
    #[cfg(feature = "fastembed")]
    pub fn load(name: &str) -> Result<Reranker> {
        use fastembed::RerankerModel as M;
        let model = [
            M::JINARerankerV1TurboEn,
            M::BGERerankerBase,
            M::JINARerankerV2BaseMultiligual,
            M::BGERerankerV2M3,
        ]
        .into_iter()
        .find(|m| format!("{m:?}").eq_ignore_ascii_case(name))
        .with_context(|| format!("unknown reranker {name}"))?;
        let opts = fastembed::RerankInitOptions::new(model)
            .with_cache_dir(crate::db::data_dir().join("models").join("fastembed"))
            .with_max_length(512)
            .with_show_download_progress(false);
        Ok(Reranker {
            inner: std::sync::Mutex::new(fastembed::TextRerank::try_new(opts)?),
            name: name.to_string(),
        })
    }

    #[cfg(not(feature = "fastembed"))]
    pub fn load(name: &str) -> Result<Reranker> {
        anyhow::bail!("reranker {name} needs a build with --features fastembed")
    }

    /// Relevance of each document to the query, 0 to 1, in the documents' order.
    #[cfg(feature = "fastembed")]
    pub fn scores(&self, query: &str, docs: &[String]) -> Result<Vec<f32>> {
        if docs.is_empty() {
            return Ok(vec![]);
        }
        let mut m = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("reranker poisoned"))?;
        let docs: Vec<&str> = docs.iter().map(String::as_str).collect();
        let results = m.rerank(query, docs.as_slice(), false, Some(16))?;
        let mut out = vec![0.0; docs.len()];
        for r in results {
            out[r.index] = 1.0 / (1.0 + (-r.score).exp());
        }
        Ok(out)
    }

    #[cfg(not(feature = "fastembed"))]
    pub fn scores(&self, _query: &str, _docs: &[String]) -> Result<Vec<f32>> {
        anyhow::bail!("reranking needs a build with --features fastembed")
    }
}
