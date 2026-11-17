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

const BOOTSTRAP_FILES: [&str; 3] = ["config.toml", "AGENTS.md", "AGENTS.override.md"];
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
    // stays private; the rotating OAuth credential is shared as one file.
    for name in LINKED_DIRS {
        if existing.contains(name) {
            continue;
        }
        tokio::fs::symlink(source.join(name), home.join(name)).await?;
    }
    share_auth_file(&source, &home).await?;
    if let Some(session_id) = session_id.filter(|id| valid_session_id(id)) {
        import_session(&source, &home, session_id).await?;
    }
    Ok(home.to_string_lossy().into_owned())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AuthKind {
    OAuth,
    ApiKey,
    Other,
}

struct AuthRecord {
    kind: AuthKind,
    refresh: Option<i64>,
    account: Option<String>,
}

async fn read_auth_record(path: &Path) -> Option<AuthRecord> {
    let value: serde_json::Value =
        serde_json::from_slice(&tokio::fs::read(path).await.ok()?).ok()?;
    let mode = value.get("auth_mode").and_then(serde_json::Value::as_str);
    let api_key = value
        .get("OPENAI_API_KEY")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|key| !key.is_empty());
    let tokens = value.get("tokens").and_then(serde_json::Value::as_object);
    let oauth_tokens = tokens.is_some_and(|tokens| {
        ["refresh_token", "access_token"].iter().any(|key| {
            tokens
                .get(*key)
                .and_then(serde_json::Value::as_str)
                .is_some_and(|token| !token.is_empty())
        })
    });
    let kind = match mode {
        Some("chatgpt" | "chatgptAuthTokens") => AuthKind::OAuth,
        Some("apikey") => AuthKind::ApiKey,
        Some(_) => AuthKind::Other,
        None if api_key && !oauth_tokens => AuthKind::ApiKey,
        None if oauth_tokens && !api_key => AuthKind::OAuth,
        None => AuthKind::Other,
    };
    let refresh = value
        .get("last_refresh")
        .and_then(serde_json::Value::as_str)
        .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.timestamp_millis());
    let account = tokens
        .and_then(|tokens| tokens.get("account_id"))
        .and_then(serde_json::Value::as_str)
        .filter(|account| !account.is_empty())
        .map(String::from);
    Some(AuthRecord {
        kind,
        refresh,
        account,
    })
}

fn should_publish_private_auth(desktop: &AuthRecord, private: &AuthRecord) -> bool {
    desktop.kind == AuthKind::OAuth
        && private.kind == AuthKind::OAuth
        && desktop
            .account
            .as_ref()
            .is_some_and(|account| !account.is_empty())
        && desktop.account == private.account
        && matches!((desktop.refresh, private.refresh), (Some(desktop), Some(private)) if private > desktop)
}

async fn share_auth_file(source_home: &Path, home: &Path) -> Result<(), AcpError> {
    let source = source_home.join("auth.json");
    let target = home.join("auth.json");
    let (source_auth, target_auth) =
        tokio::join!(read_auth_record(&source), read_auth_record(&target));
    let Some(source_auth) = source_auth else {
        return Ok(());
    };
    if tokio::fs::symlink_metadata(&target)
        .await
        .map(|meta| meta.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Ok(());
    }
    if let Some(target_auth) = target_auth {
        if should_publish_private_auth(&source_auth, &target_auth) {
            let destination = tokio::fs::canonicalize(&source)
                .await
                .unwrap_or_else(|_| source.clone());
            write_private(&destination, &tokio::fs::read(&target).await?, 0o600).await?;
        }
    }
    let temporary = tmp_sibling(&target);
    let result = async {
        tokio::fs::symlink(&source, &temporary).await?;
        tokio::fs::rename(&temporary, &target).await?;
        Ok::<(), std::io::Error>(())
    }
    .await;
    if result.is_err() {
        write_private(&target, &tokio::fs::read(&source).await?, 0o600).await?;
    }
    let _ = tokio::fs::remove_file(&temporary).await;
    Ok(())
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
