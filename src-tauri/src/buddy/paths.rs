//! Filesystem layout, port of `src/main/buddy/paths.ts`.

use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct BuddyPaths {
    pub data_root: PathBuf,
    pub global_settings: PathBuf,
    pub workspaces_dir: PathBuf,
    pub runtime_tasks_dir: PathBuf,
}

pub fn create_buddy_paths(data_root: &Path) -> BuddyPaths {
    BuddyPaths {
        data_root: data_root.to_path_buf(),
        global_settings: data_root.join("global").join("settings.json"),
        workspaces_dir: data_root.join("workspaces"),
        runtime_tasks_dir: data_root.join("runtime").join("tasks"),
    }
}

/// Slug + short sha256 digest of the resolved repo path. Matches the Electron
/// edition exactly so workspaces are shared between both apps.
pub fn workspace_key_for_repo(repo_root: &str) -> String {
    let root = resolve_path(repo_root);
    let root_str = root.to_string_lossy();
    let base = root
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "root".to_string());
    let mut slug: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    // Collapse runs of '-' produced by the regex [^a-zA-Z0-9._-]+
    while slug.contains("--") {
        slug = slug.replace("--", "-");
    }
    let slug = slug
        .trim_matches(|c| c == '.' || c == '_' || c == '-')
        .chars()
        .take(40)
        .collect::<String>();
    let slug = if slug.is_empty() { "workspace" } else { &slug };
    let digest = hex_sha256(root_str.as_bytes());
    format!("{}-{}", slug, &digest[..12])
}

pub fn workspace_dir(paths: &BuddyPaths, workspace_key: &str) -> PathBuf {
    paths.workspaces_dir.join(workspace_key)
}

pub fn task_dir(paths: &BuddyPaths, workspace_key: &str, task_id: &str) -> PathBuf {
    workspace_dir(paths, workspace_key)
        .join("tasks")
        .join(task_id)
}

pub fn canonical_repo_root(repo_root: &str) -> PathBuf {
    let root = resolve_path(repo_root);
    std::fs::canonicalize(&root).unwrap_or(root)
}

fn resolve_path(p: &str) -> PathBuf {
    let path = Path::new(p);
    if path.is_absolute() {
        normalize_components(path.to_path_buf())
    } else {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        normalize_components(cwd.join(path))
    }
}

/// Lexical path normalization equivalent to Node's `path.resolve` for absolute
/// paths (handles `.` and `..` without touching the filesystem).
fn normalize_components(path: PathBuf) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn hex_sha256(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    let digest = hasher.finalize();
    digest.iter().map(|b| format!("{:02x}", b)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_key_matches_electron_format() {
        let key = workspace_key_for_repo("/tmp/my repo");
        // slug: "my-repo", suffix: 12 hex chars
        assert!(key.starts_with("my-repo-"));
        let suffix = key.rsplit('-').next().unwrap();
        assert_eq!(suffix.len(), 12);
        assert!(suffix.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn empty_slug_falls_back_to_workspace() {
        let key = workspace_key_for_repo("/.../___");
        assert!(key.contains('-'));
    }
}
