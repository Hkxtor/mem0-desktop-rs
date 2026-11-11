//! `GET /diagnostics/storage` — why the memory pages are empty.
//!
//! "No memories yet" is what the dashboard shows for an empty list, but an empty list has
//! several very different causes and only one of them means "nothing was ever added": the
//! runtime may not be configured, the data directory may be a different one than the
//! operator expects, or the directory may still hold the Python build's `duckdb/` store,
//! which this app cannot read. Those need different actions, so the pages ask here rather
//! than guessing.

use std::sync::Arc;

use axum::extract::State;
use axum::response::Json;
use axum::Extension;
use serde_json::{json, Value};

use super::auth::AuthContext;
use super::error::{ApiError, ApiResult};
use super::AppState;

/// Files the Python build leaves behind in the data directory.
const LEGACY_DIR: &str = "duckdb";

pub async fn storage_diagnostics(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
) -> ApiResult<Json<Value>> {
    if !auth.is_admin() {
        return Err(ApiError::forbidden("Admin role required."));
    }

    let data_dir = state.data_dir.clone();
    let vectors_db = data_dir.join("vectors.db");
    let runtime = state.memory.read().await.clone();
    let config_error = state
        .config_error
        .lock()
        .ok()
        .and_then(|guard| guard.clone());

    // One row is enough: the question is "empty or not", not "how many".
    let mut has_memories = false;
    if let Some(mem) = &runtime {
        has_memories = mem
            .vector_store()
            .list(&Default::default(), Some(1))
            .await
            .map(|hits| !hits.is_empty())
            .unwrap_or(false);
    }

    let legacy = legacy_files(&data_dir);
    let hint = hint_for(
        &runtime.is_some(),
        &config_error,
        has_memories,
        &legacy,
        &data_dir,
    );

    Ok(Json(json!({
        "data_dir": data_dir.display().to_string(),
        "vectors_db": vectors_db.display().to_string(),
        "vectors_db_exists": vectors_db.exists(),
        "runtime_configured": runtime.is_some(),
        "config_error": config_error,
        "has_memories": has_memories,
        "legacy_python_data": legacy,
        "hint": hint,
    })))
}

/// Names of the Python build's vector files, relative to the data directory.
fn legacy_files(data_dir: &std::path::Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(data_dir.join(LEGACY_DIR)) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .filter(|p| {
            p.extension()
                .and_then(|e| e.to_str())
                .map(|e| matches!(e, "db" | "duckdb" | "wal"))
                .unwrap_or(false)
        })
        .filter_map(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| format!("{LEGACY_DIR}/{n}"))
        })
        .collect();
    names.sort();
    names
}

fn hint_for(
    runtime_configured: &bool,
    config_error: &Option<String>,
    has_memories: bool,
    legacy: &[String],
    data_dir: &std::path::Path,
) -> Option<String> {
    if !runtime_configured {
        return Some(match config_error {
            Some(err) => format!(
                "No provider runtime could be built, so memories cannot be read: {err}. Fix the configuration and reload."
            ),
            None => "No provider is configured yet, so memories cannot be read. Open /setup to configure one.".to_string(),
        });
    }
    if has_memories {
        return None;
    }
    if !legacy.is_empty() {
        return Some(format!(
            "This data directory holds memories from the Python build ({}), which this app cannot read: they are in DuckDB files, while this build stores them in vectors.db. Export them as JSON from the Python app, then import them here from the Export page.",
            legacy.join(", ")
        ));
    }
    Some(format!(
        "The memory store in {} is empty. Memories added with a different data directory will not appear here; MEM0_DATA_DIR decides which directory is used.",
        data_dir.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn an_unconfigured_runtime_points_at_setup() {
        let d = dir();
        let hint = hint_for(&false, &None, false, &[], d.path()).unwrap();
        assert!(hint.contains("/setup"), "{hint}");
    }

    #[test]
    fn a_build_failure_is_quoted_rather_than_hidden() {
        let d = dir();
        let err = Some("[CFG_001] Unsupported LLM provider: xyz".to_string());
        let hint = hint_for(&false, &err, false, &[], d.path()).unwrap();
        assert!(hint.contains("Unsupported LLM provider"), "{hint}");
    }

    #[test]
    fn python_build_data_is_named_as_the_reason() {
        let d = dir();
        let legacy = vec!["duckdb/mem0.db".to_string()];
        let hint = hint_for(&true, &None, false, &legacy, d.path()).unwrap();
        assert!(hint.contains("duckdb/mem0.db"), "{hint}");
        assert!(hint.contains("import"), "{hint}");
    }

    #[test]
    fn an_empty_store_reports_which_directory_it_looked_at() {
        let d = dir();
        let hint = hint_for(&true, &None, false, &[], d.path()).unwrap();
        assert!(hint.contains(&d.path().display().to_string()), "{hint}");
        assert!(hint.contains("MEM0_DATA_DIR"), "{hint}");
    }

    #[test]
    fn memories_present_means_no_hint() {
        let d = dir();
        assert!(hint_for(&true, &None, true, &[], d.path()).is_none());
    }

    #[test]
    fn legacy_detection_ignores_unrelated_files_and_lists_vector_files() {
        let d = dir();
        std::fs::create_dir_all(d.path().join(LEGACY_DIR)).unwrap();
        std::fs::write(d.path().join(LEGACY_DIR).join("mem0.db"), b"x").unwrap();
        std::fs::write(d.path().join(LEGACY_DIR).join("notes.txt"), b"x").unwrap();
        assert_eq!(legacy_files(d.path()), vec!["duckdb/mem0.db".to_string()]);
    }

    #[test]
    fn legacy_detection_is_empty_without_the_directory() {
        let d = dir();
        assert!(legacy_files(d.path()).is_empty());
    }
}
