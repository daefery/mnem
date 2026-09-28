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
    /// Never use models from these providers (the endpoint's `owned_by`, e.g. "antigravity").
    #[serde(default)]
    pub exclude_providers: Vec<String>,
    /// Never use models whose id contains any of these (case-insensitive), e.g. "gemini".
    #[serde(default)]
    pub exclude_models: Vec<String>,
}

pub static CONFIG: LazyLock<Config> = LazyLock::new(|| {
    let path = db::data_dir().join("config.json");
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
