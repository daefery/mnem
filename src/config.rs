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
}

/// Background distillation through any OpenAI-compatible endpoint.
#[derive(Debug, Default, Deserialize)]
pub struct DistillConfig {
    /// Default: http://127.0.0.1:8317/v1 (CLIProxyAPI)
    pub base_url: Option<String>,
    /// Default: gpt-5.6-luna
    pub model: Option<String>,
    /// Read the API key from this environment variable...
    pub api_key_env: Option<String>,
    /// ...or from a field of a JSON file (e.g. "~/.pi/agent/cliproxyapi.json").
    pub api_key_json: Option<String>,
    /// Field name in `api_key_json` (default "apiKey").
    pub api_key_field: Option<String>,
    /// Distil automatically after each agent turn (Stop hook). Default true.
    pub on_stop: Option<bool>,
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
