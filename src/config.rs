//! Optional user configuration: `$MNEM_HOME/config.json` (default `~/.mnem/config.json`).
//!
//! ```json
//! { "harness_prompts": ["^: Firstmate instruction waiting"] }
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
