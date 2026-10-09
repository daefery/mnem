//! Optional user configuration: `$RAVNORI_HOME/config.json` (default `~/.ravnori/config.json`).
//!
//! ```json
//! {
//!   "harness_prompts": ["^: Orchestrator instruction waiting"],
//!   "distill": { "model": "gpt-5.6-luna", "api_key_json": "~/.config/my-proxy/key.json" }
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
    /// Regexes for prompts another agent's brief sends (a review council, a test run). A
    /// session with a matching prompt is scripted: not distilled, not offered memories,
    /// and its memories stay out of recall and search (see `scripted`).
    #[serde(default)]
    pub scripted_sessions: Vec<String>,
    #[serde(default)]
    pub distill: DistillConfig,
    /// Projects never captured (substring of the project id, e.g. "github.com/me/secret").
    #[serde(default)]
    pub exclude_projects: Vec<String>,
    #[serde(default)]
    pub semantic: SemanticConfig,
    #[serde(default)]
    pub recall: RecallConfig,
    /// Port of the viewer and embedding service run by `rvn watch` (default 37777).
    pub ui_port: Option<u16>,
    /// When this process read the settings (ms): a long-running process keeps what it
    /// read at start.
    #[serde(skip)]
    pub loaded_at: i64,
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

/// What prompt recall shows.
#[derive(Debug, Default, Deserialize)]
pub struct RecallConfig {
    /// A presentation trial: sessions in the trial half (`recall::trial_arm`) are shown at
    /// most this many memories per prompt; the other half keep the usual five. Unset: no
    /// trial, every session gets five.
    pub trial_top: Option<usize>,
}

/// Background distillation through any OpenAI-compatible endpoint.
#[derive(Debug, Default, Deserialize)]
pub struct DistillConfig {
    /// Where requests go: "openai" (an OpenAI-compatible endpoint, the default),
    /// "claude-cli" (`claude -p`) or "codex-cli" (`codex exec`), signed in as the user
    /// already is. `rvn install` picks one when nothing is configured.
    pub provider: Option<String>,
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
    /// ...or from a field of a JSON file (e.g. "~/.config/my-proxy/key.json").
    pub api_key_json: Option<String>,
    /// Field name in `api_key_json` (default "apiKey").
    pub api_key_field: Option<String>,
    /// Distil automatically after each agent turn (Stop hook). Default true.
    pub on_stop: Option<bool>,
    /// The watcher also distils sessions it missed, oldest first, up to this many days
    /// back. Default 7.
    pub backfill_days: Option<i64>,
    /// At most this many requests per 24 hours by all background distillation (Stop hook,
    /// watcher, backfill). Default 300 (`rvn install` sets 100 for a sign-in); 0 turns
    /// backfill off and leaves the rest unlimited. A manual `rvn distill` is not limited.
    pub daily_calls: Option<usize>,
    /// Never use models from these providers (the endpoint's `owned_by`, e.g. "antigravity").
    #[serde(default)]
    pub exclude_providers: Vec<String>,
    /// Never use models whose id contains any of these (case-insensitive), e.g. "gemini".
    #[serde(default)]
    pub exclude_models: Vec<String>,
}

/// The settings file: $RAVNORI_CONFIG when set (the recall gate evaluates a candidate
/// config this way), else config.json in ravnori's data directory.
pub fn path() -> std::path::PathBuf {
    crate::db::env_var("CONFIG")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| db::data_dir().join("config.json"))
}

pub static CONFIG: LazyLock<Config> = LazyLock::new(|| {
    let path = path();
    let mut c: Config = match std::fs::read_to_string(&path) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
            eprintln!("ravnori: ignoring {}: {e}", path.display());
            Config::default()
        }),
        Err(_) => Config::default(),
    };
    c.loaded_at = db::now_ms();
    c
});

pub static HARNESS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    CONFIG
        .harness_prompts
        .iter()
        .filter_map(|p| match Regex::new(p) {
            Ok(r) => Some(r),
            Err(e) => {
                eprintln!("ravnori: bad harness_prompts pattern {p:?}: {e}");
                None
            }
        })
        .collect()
});
