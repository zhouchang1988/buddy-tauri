//! Run lock files, port of `src/main/buddy/locks.ts`.

use serde::Serialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Serialize)]
struct RunLockFile<'a> {
    workspace_key: &'a str,
    task_id: &'a str,
    run_id: &'a str,
    pid: u32,
    app: &'static str,
    started_at: String,
}

pub async fn create_run_lock(
    data_root: &Path,
    workspace_key: &str,
    task_id: &str,
    run_id: &str,
    pid: u32,
) -> std::io::Result<PathBuf> {
    let dir = data_root.join("runtime").join("tasks");
    tokio::fs::create_dir_all(&dir).await?;
    let path = dir.join(format!("{}__{}.lock", workspace_key, task_id));
    let body = RunLockFile {
        workspace_key,
        task_id,
        run_id,
        pid,
        app: "buddy",
        started_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
    };
    let json = serde_json::to_string(&body).expect("run lock serialization");
    tokio::fs::write(&path, json).await?;
    Ok(path)
}

pub async fn remove_run_lock(path: &Path) -> std::io::Result<()> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}
