//! Optional user configuration: `$MNEM_HOME/config.json` (default `~/.mnem/config.json`).
//!
//! ```json
//! {
//!   "harness_prompts": ["^: Firstmate instruction waiting"],
//!   "distill": { "model": "gpt-5.6-luna", "api_key_json": "~/.pi/agent/cliproxyapi.json" }
//! }
//! ```

use crate::db;
use regex::Regex;
use serde::Deserialize;
use std::sync::LazyLock;

#[derive(Debug, Default, Deserialize)]
pub struct Config {
    /// Regexes for prompts injected by tooling (orchestrators, pollers). Matching prompts
    /// are labelled "harness": kept for context, excluded from "what the user asked".
    #[serde(default)]
    pub harness_prompts: Vec<String>,
    #[serde(default)]
    pub distill: DistillConfig,
    /// Projects never captured (substring of the project id, e.g. "github.com/me/secret").
    #[serde(default)]
    pub exclude_projects: Vec<String>,
    #[serde(default)]
    pub semantic: SemanticConfig,
    /// Port of the viewer and embedding service run by `mnem watch` (default 37777).
    pub ui_port: Option<u16>,
}

/// Local semantic recall.
#[derive(Debug, Default, Deserialize)]
pub struct SemanticConfig {
    /// Model2Vec model on Hugging Face (default minishlab/potion-base-8M).
    pub model: Option<String>,
    /// Set false to use keyword recall only.
    pub enabled: Option<bool>,
    /// Similarity thresholds depend on the model; defaults suit potion-base-8M.
    /// Prompt recall drops keyword hits below this cosine (default 0.45).
    pub relevance_cosine: Option<f32>,
    /// Meaning-only matches must reach this to fill a slot (default 0.55).
    pub fill_cosine: Option<f32>,
    /// Search drops any-word matches below this (default 0.35).
    pub search_cosine: Option<f32>,
}

/// Background distillation through any OpenAI-compatible endpoint.
#[derive(Debug, Default, Deserialize)]
pub struct DistillConfig {
    /// Default: http://127.0.0.1:8317/v1 (CLIProxyAPI)
    pub base_url: Option<String>,
    /// A single preferred model, tried first (kept for older configs).
    pub model: Option<String>,
    /// Ordered model chain. Default: luna, gemini flash-lite, claude haiku, terra, gemini flash.
    pub models: Option<Vec<String>>,
    /// When every chained model is exhausted or unavailable, try any other text model
    /// the endpoint lists, cheapest-looking first. Default true.
    pub auto_fallback: Option<bool>,
    /// Read the API key from this environment variable...
    pub api_key_env: Option<String>,
    /// ...or from a field of a JSON file (e.g. "~/.pi/agent/cliproxyapi.json").
    pub api_key_json: Option<String>,
    /// Field name in `api_key_json` (default "apiKey").
    pub api_key_field: Option<String>,
    /// Distil automatically after each agent turn (Stop hook). Default true.
    pub on_stop: Option<bool>,
    /// The watcher also distils sessions it missed, oldest first, up to this many days
    /// back. Default 7.
    pub backfill_days: Option<i64>,
    /// At most this many backfill calls per 24 hours. Default 300; 0 turns backfill off.
    pub daily_calls: Option<usize>,
    /// Never use models from these providers (the endpoint's `owned_by`, e.g. "antigravity").
    #[serde(default)]
    pub exclude_providers: Vec<String>,
    /// Never use models whose id contains any of these (case-insensitive), e.g. "gemini".
    #[serde(default)]
    pub exclude_models: Vec<String>,
}

/// The settings file: $MNEM_CONFIG when set (the recall gate evaluates a candidate
/// config this way), else config.json in mnem's data directory.
pub fn path() -> std::path::PathBuf {
    std::env::var_os("MNEM_CONFIG")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| db::data_dir().join("config.json"))
}

pub static CONFIG: LazyLock<Config> = LazyLock::new(|| {
    let path = path();
    match std::fs::read_to_string(&path) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
            eprintln!("mnem: ignoring {}: {e}", path.display());
            Config::default()
        }),
        Err(_) => Config::default(),
    }
});

pub static HARNESS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    CONFIG
        .harness_prompts
        .iter()
        .filter_map(|p| match Regex::new(p) {
            Ok(r) => Some(r),
            Err(e) => {
                eprintln!("mnem: bad harness_prompts pattern {p:?}: {e}");
                None
            }
        })
        .collect()
});
