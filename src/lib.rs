pub mod adapters;
pub mod agents;
pub mod api;
pub mod ask;
pub mod backup;
pub mod cli_llm;
pub mod config;
pub mod context;
pub mod db;
pub mod distill;
pub mod doctor;
pub mod embed;
pub mod eval;
pub mod files;
pub mod forget;
pub mod gate;
pub mod gitstate;
pub mod health;
pub mod history_eval;
pub mod hook;
pub mod import;
pub mod ingest;
pub mod install;
pub mod mcp;
pub mod merge;
pub mod model;
pub mod privacy;
pub mod project;
pub mod recall;
pub mod rename;
pub mod rerank;
pub mod scripted;
pub mod search;
pub mod service;
pub mod text;
pub mod trace;
pub mod ui;
pub mod uptake;
pub mod when;

/// A scratch directory for a test, removed when it is dropped (also when the test fails,
/// since a panic unwinds through the drop). Tests used to leave theirs in the system temp
/// folder, one per run, until it filled.
#[doc(hidden)]
pub struct TempDir(std::path::PathBuf);

impl TempDir {
    /// `<temp>/ravnori-<name>-<pid>-<n>`, empty and created.
    pub fn new(name: &str) -> TempDir {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("ravnori-{name}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).expect("create a test directory");
        TempDir(p)
    }

    pub fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl std::ops::Deref for TempDir {
    type Target = std::path::Path;
    fn deref(&self) -> &std::path::Path {
        &self.0
    }
}

impl AsRef<std::path::Path> for TempDir {
    fn as_ref(&self) -> &std::path::Path {
        &self.0
    }
}

impl AsRef<std::ffi::OsStr> for TempDir {
    fn as_ref(&self) -> &std::ffi::OsStr {
        self.0.as_os_str()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
