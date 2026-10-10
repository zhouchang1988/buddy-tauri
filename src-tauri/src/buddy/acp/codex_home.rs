//! Isolated Codex home for the ACP channel, port of
//! `src/main/buddy/acp/codex-home.ts`.
//!
//! Copy only bootstrap configuration. Never share session directories or
//! state DBs with the desktop app; WeCode can also rewrite its private
//! configuration safely.

use crate::buddy::acp::types::AcpError;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, OnceLock};
use tokio::sync::Mutex;

const BOOTSTRAP_FILES: [&str; 4] = [
    "config.toml",
    "auth.json",
    "AGENTS.md",
    "AGENTS.override.md",
];
const LINKED_DIRS: [&str; 2] = ["skills", "rules"];

fn home_gates() -> &'static Mutex<HashMap<String, Arc<Mutex<()>>>> {
    static GATES: OnceLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> = OnceLock::new();
    GATES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// CLI overrides take precedence over `sqlite_home` copied from desktop config.
pub async fn codex_isolation_wrapper(data_root: &Path) -> Result<String, AcpError> {
    let directory = data_root.join("bin");
    tokio::fs::create_dir_all(&directory).await?;
    set_mode(&directory, 0o700)?;
    let wrapper = directory.join("buddy-codex-acp");
    write_private(
        &wrapper,
        b"#!/bin/bash\nexec \"$BUDDY_CODEX_EXECUTABLE\" -c \"$BUDDY_CODEX_SQLITE_CONFIG\" \"$@\"\n",
        0o700,
    )
    .await?;
    Ok(wrapper.to_string_lossy().into_owned())
}

/// Stable per-actor home, shared by runs, probes and launcher checks.
///
/// An explicitly configured launcher `CODEX_HOME` remains under the user's
/// control and is returned unchanged.
pub async fn prepare_codex_home(
    actor: &str,
    data_root: &Path,
    env: &HashMap<String, String>,
    session_id: Option<&str>,
    source_home: Option<&Path>,
) -> Result<String, AcpError> {
    if let Some(configured) = non_empty(env, "CODEX_HOME") {
        return Ok(configured);
    }
    let source = match source_home {
        Some(path) => path.to_path_buf(),
        None => std::env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| {
                dirs::home_dir()
                    .unwrap_or_else(|| PathBuf::from("."))
                    .join(".codex")
            }),
    };
    let home = data_root.join("acp").join(actor).join("codex-home");
    if lexical_absolute(&home) == lexical_absolute(&source) {
        return Err(AcpError::Message(
            "Buddy Codex home must be isolated".into(),
        ));
    }

    let key = home.to_string_lossy().into_owned();
    let gate = {
        let mut gates = home_gates().lock().await;
        gates
            .entry(key)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    };
    let _guard = gate.lock().await;

    tokio::fs::create_dir_all(&home).await?;
    set_mode(&home, 0o700)?;
    let existing = read_names(&home).await?;
    for name in BOOTSTRAP_FILES {
        if existing.contains(name) {
            continue;
        }
        match copy_private_file(&source.join(name), &home.join(name)).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    // Skills and rules retain their original relative resources. Runtime state
    // and credentials are deliberately not linked.
    for name in LINKED_DIRS {
        if existing.contains(name) {
            continue;
        }
        tokio::fs::symlink(source.join(name), home.join(name)).await?;
    }
    if let Some(session_id) = session_id.filter(|id| valid_session_id(id)) {
        import_session(&source, &home, session_id).await?;
    }
    Ok(home.to_string_lossy().into_owned())
}

async fn import_session(source_home: &Path, home: &Path, session_id: &str) -> Result<(), AcpError> {
    // Codex resumes by finding the rollout under sessions. Import just the
    // selected legacy thread, never the shared SQLite index.
    let suffix = format!("-{session_id}.jsonl");
    let mut stack = vec![PathBuf::from("sessions")];
    while let Some(relative) = stack.pop() {
        let directory = source_home.join(&relative);
        let mut entries = match tokio::fs::read_dir(&directory).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        while let Some(entry) = entries.next_entry().await? {
            let path = relative.join(entry.file_name());
            let file_type = entry.file_type().await?;
            if file_type.is_dir() {
                stack.push(path);
            } else if file_type.is_file() && entry.file_name().to_string_lossy().ends_with(&suffix)
            {
                let destination_dir = home.join(&relative);
                tokio::fs::create_dir_all(&destination_dir).await?;
                set_mode(&destination_dir, 0o700)?;
                let name = entry.file_name();
                if read_names(&destination_dir)
                    .await?
                    .contains(name.to_string_lossy().as_ref())
                {
                    continue;
                }
                copy_private_file(&source_home.join(&path), &home.join(&path)).await?;
            }
        }
    }
    Ok(())
}

async fn copy_private_file(source: &Path, target: &Path) -> std::io::Result<()> {
    let bytes = tokio::fs::read(source).await?;
    write_private(target, &bytes, 0o600).await
}

async fn write_private(target: &Path, bytes: &[u8], mode: u32) -> std::io::Result<()> {
    let temporary = tmp_sibling(target);
    let write_result = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(mode);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&temporary, target)?;
        set_mode(target, mode)?;
        Ok(())
    })();
    let _ = std::fs::remove_file(&temporary);
    write_result
}

fn tmp_sibling(target: &Path) -> PathBuf {
    let name = target
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("file");
    target.with_file_name(format!("{name}.{}.tmp", uuid::Uuid::new_v4()))
}

fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
        Ok(())
    }
}

async fn read_names(directory: &Path) -> Result<HashSet<String>, AcpError> {
    let mut names = HashSet::new();
    let mut entries = tokio::fs::read_dir(directory).await?;
    while let Some(entry) = entries.next_entry().await? {
        names.insert(entry.file_name().to_string_lossy().into_owned());
    }
    Ok(names)
}

fn non_empty(env: &HashMap<String, String>, key: &str) -> Option<String> {
    env.get(key).filter(|value| !value.is_empty()).cloned()
}

fn valid_session_id(session_id: &str) -> bool {
    !session_id.is_empty()
        && session_id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
}

/// `path.resolve` without touching the filesystem: absolute, `.` / `..` collapsed.
fn lexical_absolute(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("/"))
            .join(path)
    };
    let mut out = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}
