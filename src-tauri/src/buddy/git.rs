//! Git integration — Rust port of `src/main/buddy/git.ts`.
//!
//! Thin async wrappers around the `git` CLI (spawned via
//! `tokio::process::Command`). All parsing is byte-for-byte compatible with
//! the TypeScript original. Commit-message generation lives in
//! `commit_message.rs` (upstream moved it out of git.ts in v1.2.11);
//! [`git_diff_for_commit_message`] only delegates there.

use std::path::Path;
use std::time::Duration;

use regex::Regex;
use serde::Serialize;
use tokio::process::Command;

use super::commit_message;
use super::types::{GitDiffStats, GitFileStatus, GitRemote, GitStatusResult};

/// Result of `gitCommitAndPush`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitCommitResult {
    pub commit_hash: String,
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct GitError(pub String);

impl From<std::io::Error> for GitError {
    fn from(err: std::io::Error) -> Self {
        GitError(err.to_string())
    }
}

type GitResult<T> = Result<T, GitError>;

// ---------------------------------------------------------------------------
// exec_git — port of `execGit` (stdout trimmed; one retry on index.lock)
// ---------------------------------------------------------------------------

/// Delete `.git/index.lock` when it is older than `max_age_ms` (port of
/// `removeStaleIndexLock`, default 10s).
fn remove_stale_index_lock(cwd: &str, max_age_ms: u64) {
    let lock_path = Path::new(cwd).join(".git").join("index.lock");
    let Ok(metadata) = std::fs::metadata(&lock_path) else {
        return;
    };
    let Ok(modified) = metadata.modified() else {
        return;
    };
    let age_ms = modified.elapsed().map(|d| d.as_millis() as u64).unwrap_or(0);
    if age_ms > max_age_ms {
        // Lock file might have been removed between check and delete — ignore.
        let _ = std::fs::remove_file(&lock_path);
    }
}

async fn exec_git_inner(args: &[&str], cwd: &str) -> GitResult<String> {
    let output = Command::new("git")
        // `core.quotepath=false` makes git print non-ASCII paths (e.g. Chinese
        // file names) as UTF-8 instead of C-style octal escapes like
        // "\346\226\207.md" in status/diff output.
        .arg("-c")
        .arg("core.quotepath=false")
        .args(args)
        .current_dir(cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .await?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let code = output
            .status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "null".to_string());
        let message = if stderr.is_empty() {
            format!("git {} exited with {}", args.join(" "), code)
        } else {
            stderr
        };
        return Err(GitError(message));
    }
    Ok(stdout)
}

async fn exec_git(args: &[&str], cwd: &str, retries: u32) -> GitResult<String> {
    match exec_git_inner(args, cwd).await {
        Err(err) if retries > 0 && err.0.contains("index.lock") => {
            remove_stale_index_lock(cwd, 10_000);
            tokio::time::sleep(Duration::from_millis(500)).await;
            Box::pin(exec_git(args, cwd, retries - 1)).await
        }
        other => other,
    }
}

// ---------------------------------------------------------------------------
// Read-only queries
// ---------------------------------------------------------------------------

fn numstat_regex() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(\d+)\s+(\d+)\s+(.+)$").unwrap())
}

/// Port of `parseDiffStat`.
fn parse_diff_stat(output: &str) -> Option<GitDiffStats> {
    if output.is_empty() {
        return None;
    }
    let mut files: Vec<GitFileStatus> = Vec::new();
    let mut files_changed = 0u64;
    let mut insertions = 0u64;
    let mut deletions = 0u64;
    for line in output.split('\n') {
        if let Some(caps) = numstat_regex().captures(line) {
            files_changed += 1;
            let ins: u64 = caps[1].parse().unwrap_or(0);
            let del: u64 = caps[2].parse().unwrap_or(0);
            insertions += ins;
            deletions += del;
            files.push(GitFileStatus {
                path: caps[3].trim().to_string(),
                status: "M".to_string(),
                insertions: ins,
                deletions: del,
            });
        }
    }
    if files_changed == 0 {
        return None;
    }
    Some(GitDiffStats {
        files_changed,
        insertions,
        deletions,
        summary: output.to_string(),
        files: Some(files),
    })
}

pub async fn get_git_branch(cwd: &str) -> String {
    exec_git(&["rev-parse", "--abbrev-ref", "HEAD"], cwd, 1)
        .await
        .unwrap_or_default()
}

pub async fn get_git_diff_stats(cwd: &str) -> Option<GitDiffStats> {
    let output = exec_git(&["diff", "--numstat", "--no-renames"], cwd, 1)
        .await
        .ok()?;
    parse_diff_stat(&output)
}

pub async fn get_git_staged_stats(cwd: &str) -> Option<GitDiffStats> {
    let output = exec_git(&["diff", "--cached", "--numstat", "--no-renames"], cwd, 1)
        .await
        .ok()?;
    parse_diff_stat(&output)
}

pub async fn get_git_remotes(cwd: &str) -> Vec<GitRemote> {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"^(\S+)\s+(\S+)\s+\(fetch\)").unwrap());
    let Ok(output) = exec_git(&["remote", "-v"], cwd, 1).await else {
        return Vec::new();
    };
    let mut remotes = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for line in output.split('\n') {
        if let Some(caps) = re.captures(line) {
            let name = caps[1].to_string();
            if seen.insert(name.clone()) {
                remotes.push(GitRemote {
                    name,
                    url: caps[2].to_string(),
                });
            }
        }
    }
    remotes
}

pub async fn get_git_untracked_count(cwd: &str) -> u64 {
    let Ok(output) = exec_git(&["ls-files", "--others", "--exclude-standard"], cwd, 1).await
    else {
        return 0;
    };
    if output.trim().is_empty() {
        return 0;
    }
    output.split('\n').filter(|l| !l.is_empty()).count() as u64
}

/// Port of `normalizeStatusCode`.
fn normalize_status_code(xy: &str) -> String {
    let mut chars = xy.chars();
    let x = chars.next().unwrap_or('\0');
    let y = chars.next().unwrap_or('\0');
    let code = if x == '?' || y == '?' {
        '?'
    } else if x == 'A' || y == 'A' {
        'A'
    } else if x == 'D' || y == 'D' {
        'D'
    } else if x == 'R' || y == 'R' {
        'R'
    } else if x == 'C' || y == 'C' {
        'C'
    } else {
        'M'
    };
    code.to_string()
}

pub async fn get_git_file_statuses(cwd: &str) -> Vec<GitFileStatus> {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"^([MADRCU? ]{1,2})\s+(.+)$").unwrap());
    let Ok(output) = exec_git(&["status", "--porcelain", "--no-renames"], cwd, 1).await else {
        return Vec::new();
    };
    if output.trim().is_empty() {
        return Vec::new();
    }
    let mut result = Vec::new();
    for line in output.split('\n') {
        if line.trim().is_empty() {
            continue;
        }
        let Some(caps) = re.captures(line) else {
            continue;
        };
        let xy = &caps[1];
        let file_path = caps[2].trim();
        if file_path.is_empty() {
            continue;
        }
        result.push(GitFileStatus {
            path: file_path.to_string(),
            status: normalize_status_code(xy),
            insertions: 0,
            deletions: 0,
        });
    }
    result
}

/// Port of `mergeFileStatuses`: sums numstat insertions/deletions by path
/// (unstaged first, then staged) and folds them into the porcelain listing.
fn merge_file_statuses(
    file_statuses: Vec<GitFileStatus>,
    diff_files: Option<&[GitFileStatus]>,
    staged_files: Option<&[GitFileStatus]>,
) -> Vec<GitFileStatus> {
    let mut by_path: std::collections::HashMap<String, (u64, u64)> = std::collections::HashMap::new();
    for files in [diff_files, staged_files].into_iter().flatten() {
        for f in files {
            let entry = by_path.entry(f.path.clone()).or_insert((0, 0));
            entry.0 += f.insertions;
            entry.1 += f.deletions;
        }
    }
    file_statuses
        .into_iter()
        .map(|mut f| {
            if let Some((ins, del)) = by_path.get(&f.path) {
                f.insertions = *ins;
                f.deletions = *del;
            }
            f
        })
        .collect()
}

pub async fn get_git_status(cwd: &str) -> GitStatusResult {
    let (branch, diff, staged, untracked, remotes, files) = tokio::join!(
        get_git_branch(cwd),
        get_git_diff_stats(cwd),
        get_git_staged_stats(cwd),
        get_git_untracked_count(cwd),
        get_git_remotes(cwd),
        get_git_file_statuses(cwd),
    );
    let merged_files = merge_file_statuses(
        files,
        diff.as_ref().and_then(|d| d.files.as_deref()),
        staged.as_ref().and_then(|s| s.files.as_deref()),
    );
    GitStatusResult {
        branch,
        diff,
        staged,
        untracked,
        files: merged_files,
        remotes,
    }
}

// ---------------------------------------------------------------------------
// Mutations
// ---------------------------------------------------------------------------

pub async fn git_stage_all(cwd: &str) -> GitResult<()> {
    remove_stale_index_lock(cwd, 10_000);
    exec_git(&["add", "-A"], cwd, 1).await?;
    Ok(())
}

/// Stage only the given paths: clear the index first, then stage exactly
/// these paths so the next commit contains them and nothing else.
pub async fn git_stage_files(cwd: &str, paths: &[String]) -> GitResult<()> {
    if paths.is_empty() {
        return Err(GitError("No files selected to stage".to_string()));
    }
    remove_stale_index_lock(cwd, 10_000);
    // `reset` fails on a repo without HEAD; nothing to clear then — ignore.
    let _ = exec_git(&["reset", "-q"], cwd, 1).await;
    let mut args: Vec<&str> = vec!["add", "-A", "--"];
    args.extend(paths.iter().map(String::as_str));
    exec_git(&args, cwd, 1).await?;
    Ok(())
}

pub async fn git_commit_and_push(
    cwd: &str,
    message: &str,
    remote: &str,
    push: bool,
) -> GitResult<GitCommitResult> {
    remove_stale_index_lock(cwd, 10_000);
    exec_git(&["commit", "-m", message], cwd, 1).await?;
    let commit_hash = exec_git(&["rev-parse", "--short", "HEAD"], cwd, 1).await?;
    if push {
        exec_git(&["push", remote], cwd, 1).await?;
    }
    Ok(GitCommitResult { commit_hash })
}

/// List local branch names (short form). Returns [] on error.
pub async fn git_branches(cwd: &str) -> Vec<String> {
    let Ok(output) = exec_git(&["branch", "--format=%(refname:short)"], cwd, 1).await else {
        return Vec::new();
    };
    if output.is_empty() {
        return Vec::new();
    }
    output
        .split('\n')
        .map(|b| b.trim().to_string())
        .filter(|b| !b.is_empty())
        .collect()
}

/// Switch to a local branch. Errors carry git's stderr (e.g. dirty tree).
pub async fn git_checkout(cwd: &str, branch: &str) -> GitResult<()> {
    assert_valid_branch_name(branch)?;
    exec_git(&["checkout", branch], cwd, 1).await?;
    Ok(())
}

/// Create a new branch from HEAD and switch to it.
pub async fn git_create_branch(cwd: &str, branch: &str) -> GitResult<()> {
    assert_valid_branch_name(branch)?;
    exec_git(&["checkout", "-b", branch], cwd, 1).await?;
    Ok(())
}

fn assert_valid_branch_name(branch: &str) -> GitResult<()> {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"^[^\s~^:?*\[\]\\]+$").unwrap());
    if !re.is_match(branch) || branch.starts_with('-') {
        return Err(GitError(format!("Invalid branch name: {branch}")));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Single-file diff — port of `gitFileDiff`
// ---------------------------------------------------------------------------

const MAX_DIFF_BYTES: usize = 200_000;

fn build_new_file_diff(file_path: &str, content: &str) -> String {
    let mut lines: Vec<&str> = content.split('\n').collect();
    // A trailing newline produces an empty final segment; drop it.
    if !lines.is_empty() && lines[lines.len() - 1].is_empty() {
        lines.pop();
    }
    let body = lines
        .iter()
        .map(|l| format!("+{l}"))
        .collect::<Vec<_>>()
        .join("\n");
    [
        format!("diff --git a/{file_path} b/{file_path}"),
        "new file mode 100644".to_string(),
        "--- /dev/null".to_string(),
        format!("+++ b/{file_path}"),
        format!("@@ -0,0 +1,{} @@", lines.len()),
        body,
    ]
    .join("\n")
}

/// JS `string.slice(0, MAX_DIFF_BYTES)` operates on UTF-16 code units; we cut
/// on a char boundary instead (only differs for astral-plane characters).
fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    text.chars().take(max).collect()
}

/// Unified diff for a single file (staged + unstaged vs HEAD).
/// Falls back to an all-added pseudo diff for untracked files or repos
/// without commits.
pub async fn git_file_diff(cwd: &str, file_path: &str) -> String {
    let mut diff = match exec_git(&["diff", "HEAD", "--no-renames", "--", file_path], cwd, 1).await
    {
        Ok(diff) => diff,
        Err(_) => {
            // HEAD may not exist yet (no commits); try staged + unstaged separately.
            let staged_args = ["diff", "--cached", "--no-renames", "--", file_path];
            let unstaged_args = ["diff", "--no-renames", "--", file_path];
            let (staged, unstaged) = tokio::join!(
                exec_git(&staged_args, cwd, 1),
                exec_git(&unstaged_args, cwd, 1),
            );
            [staged.unwrap_or_default(), unstaged.unwrap_or_default()]
                .into_iter()
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join("\n")
        }
    };
    if !diff.is_empty() {
        if diff.chars().count() > MAX_DIFF_BYTES {
            diff = format!("{}\n… (diff truncated)", truncate_chars(&diff, MAX_DIFF_BYTES));
        }
        return diff;
    }
    // Untracked file: synthesize an all-added diff from disk content.
    let abs = Path::new(cwd).join(file_path);
    let Ok(metadata) = std::fs::metadata(&abs) else {
        return String::new();
    };
    if !metadata.is_file() {
        return String::new();
    }
    let Ok(buf) = std::fs::read(&abs) else {
        return String::new();
    };
    if buf.contains(&0) {
        return "(binary file)".to_string();
    }
    let mut content = String::from_utf8_lossy(&buf).to_string();
    if content.chars().count() > MAX_DIFF_BYTES {
        content = format!(
            "{}\n… (file truncated)",
            truncate_chars(&content, MAX_DIFF_BYTES)
        );
    }
    build_new_file_diff(file_path, &content)
}

// ---------------------------------------------------------------------------
// Commit-message diff — port of `gitDiffForCommitMessage` (delegates to
// `commit_message.rs`; generation itself lives there since upstream v1.2.11)
// ---------------------------------------------------------------------------

/// `paths == None` means "all changes"; `Some(&[])` means "nothing selected".
pub async fn git_diff_for_commit_message(cwd: &str, paths: Option<&[String]>) -> String {
    if let Some(paths) = paths {
        if paths.is_empty() {
            return String::new();
        }
    }
    let selected_paths: Vec<String> = match paths {
        Some(paths) => paths.to_vec(),
        None => get_git_file_statuses(cwd)
            .await
            .into_iter()
            .map(|f| f.path)
            .collect(),
    };
    commit_message::git_diff_for_selected_files(cwd, &selected_paths)
        .await
        .diff
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command as StdCommand;

    /// `git init` a temp dir with an initial commit; returns the tempdir.
    fn init_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| {
            let status = StdCommand::new("git")
                .args(args)
                .current_dir(dir.path())
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .output()
                .unwrap();
            assert!(status.status.success(), "git {:?} failed: {:?}", args, status);
        };
        run(&["init", "-b", "main"]);
        run(&["config", "user.name", "Test"]);
        run(&["config", "user.email", "test@example.com"]);
        std::fs::write(dir.path().join("a.txt"), "hello\n").unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-m", "init"]);
        dir
    }

    fn path(dir: &tempfile::TempDir) -> String {
        dir.path().to_string_lossy().to_string()
    }

    #[tokio::test]
    async fn branch_and_status_in_non_repo_are_empty() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = path(&dir);
        assert_eq!(get_git_branch(&cwd).await, "");
        let status = get_git_status(&cwd).await;
        assert_eq!(status.branch, "");
        assert!(status.diff.is_none());
        assert!(status.staged.is_none());
        assert_eq!(status.untracked, 0);
        assert!(status.remotes.is_empty());
        assert!(status.files.is_empty());
    }

    #[tokio::test]
    async fn status_reports_changes_and_merges_stats() {
        let dir = init_repo();
        let cwd = path(&dir);
        std::fs::write(dir.path().join("a.txt"), "hello\nworld\n").unwrap();
        std::fs::write(dir.path().join("b.txt"), "new file\n").unwrap();

        let status = get_git_status(&cwd).await;
        assert_eq!(status.branch, "main");
        assert_eq!(status.untracked, 1);
        let diff = status.diff.expect("diff stats");
        assert_eq!(diff.files_changed, 1);
        assert_eq!(diff.insertions, 1);
        assert_eq!(diff.deletions, 0);

        let a = status.files.iter().find(|f| f.path == "a.txt").unwrap();
        assert_eq!(a.status, "M");
        assert_eq!(a.insertions, 1);
        assert_eq!(a.deletions, 0);
        let b = status.files.iter().find(|f| f.path == "b.txt").unwrap();
        assert_eq!(b.status, "?");
    }

    #[tokio::test]
    async fn status_reports_non_ascii_paths_as_utf8() {
        let dir = init_repo();
        let cwd = path(&dir);
        // Without core.quotepath=false git would print this as
        // "\346\265\213\350\257\225.txt" (C-style octal escapes).
        std::fs::write(dir.path().join("测试.txt"), "中文内容\n").unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello\n中文\n").unwrap();

        let status = get_git_status(&cwd).await;
        let untracked = status
            .files
            .iter()
            .find(|f| f.path == "测试.txt")
            .expect("untracked Chinese path should be UTF-8, not octal-escaped");
        assert_eq!(untracked.status, "?");
        let modified = status
            .files
            .iter()
            .find(|f| f.path == "a.txt")
            .expect("modified file");
        assert_eq!(modified.insertions, 1);

        // Single-file diff must also work with a UTF-8 path.
        let diff = git_file_diff(&cwd, "a.txt").await;
        assert!(diff.contains("+中文"), "{diff}");
    }

    #[tokio::test]
    async fn stage_all_and_staged_stats() {
        let dir = init_repo();
        let cwd = path(&dir);
        std::fs::write(dir.path().join("a.txt"), "hello\nworld\nagain\n").unwrap();
        git_stage_all(&cwd).await.unwrap();
        let status = get_git_status(&cwd).await;
        assert!(status.diff.is_none(), "worktree diff should be empty");
        let staged = status.staged.expect("staged stats");
        assert_eq!(staged.insertions, 2);
    }

    #[tokio::test]
    async fn stage_files_stages_exactly_the_given_paths() {
        let dir = init_repo();
        let cwd = path(&dir);
        std::fs::write(dir.path().join("a.txt"), "hello\nchanged\n").unwrap();
        std::fs::write(dir.path().join("c.txt"), "c\n").unwrap();
        git_stage_all(&cwd).await.unwrap();
        // Now only stage c.txt: index must be cleared of a.txt first.
        git_stage_files(&cwd, &["c.txt".to_string()]).await.unwrap();
        let status = get_git_status(&cwd).await;
        let staged = status.staged.expect("staged stats");
        let staged_paths: Vec<&str> = staged
            .files
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(|f| f.path.as_str())
            .collect();
        assert_eq!(staged_paths, vec!["c.txt"]);
        // a.txt is back to unstaged.
        assert!(status.diff.is_some());
        assert!(git_stage_files(&cwd, &[]).await.is_err());
    }

    #[tokio::test]
    async fn commit_without_push_returns_short_hash() {
        let dir = init_repo();
        let cwd = path(&dir);
        std::fs::write(dir.path().join("a.txt"), "hello\nworld\n").unwrap();
        git_stage_all(&cwd).await.unwrap();
        let result = git_commit_and_push(&cwd, "feat: add world", "origin", false)
            .await
            .unwrap();
        assert_eq!(result.commit_hash.len(), 7);
        let status = get_git_status(&cwd).await;
        assert!(status.files.is_empty());
    }

    #[tokio::test]
    async fn push_to_local_remote() {
        let dir = init_repo();
        let cwd = path(&dir);
        let remote_dir = tempfile::tempdir().unwrap();
        let init = StdCommand::new("git")
            .args(["init", "--bare", "-b", "main"])
            .current_dir(remote_dir.path())
            .output()
            .unwrap();
        assert!(init.status.success());
        let add = StdCommand::new("git")
            .args([
                "remote",
                "add",
                "origin",
                &remote_dir.path().to_string_lossy(),
            ])
            .current_dir(&cwd)
            .output()
            .unwrap();
        assert!(add.status.success());

        let remotes = get_git_remotes(&cwd).await;
        assert_eq!(remotes.len(), 1);
        assert_eq!(remotes[0].name, "origin");

        std::fs::write(dir.path().join("a.txt"), "hello\nworld\n").unwrap();
        git_stage_all(&cwd).await.unwrap();
        git_commit_and_push(&cwd, "feat: push me", "origin", true)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn branches_checkout_and_create() {
        let dir = init_repo();
        let cwd = path(&dir);
        assert_eq!(git_branches(&cwd).await, vec!["main".to_string()]);
        git_create_branch(&cwd, "feature/x").await.unwrap();
        assert_eq!(get_git_branch(&cwd).await, "feature/x");
        git_checkout(&cwd, "main").await.unwrap();
        assert_eq!(get_git_branch(&cwd).await, "main");
        let mut branches = git_branches(&cwd).await;
        branches.sort();
        assert_eq!(branches, vec!["feature/x".to_string(), "main".to_string()]);
        assert!(git_checkout(&cwd, "no/such-branch").await.is_err());
        assert!(git_checkout(&cwd, "-bad").await.is_err());
        assert!(git_create_branch(&cwd, "has space").await.is_err());
        assert!(git_create_branch(&cwd, "has~tilde").await.is_err());
    }

    #[tokio::test]
    async fn file_diff_for_tracked_and_untracked_files() {
        let dir = init_repo();
        let cwd = path(&dir);
        // Tracked modification → unified diff.
        std::fs::write(dir.path().join("a.txt"), "hello\nworld\n").unwrap();
        let diff = git_file_diff(&cwd, "a.txt").await;
        assert!(diff.contains("diff --git a/a.txt b/a.txt"), "{diff}");
        assert!(diff.contains("+world"));

        // Untracked text file → synthesized all-added pseudo diff.
        std::fs::write(dir.path().join("new.txt"), "line1\nline2\n").unwrap();
        let diff = git_file_diff(&cwd, "new.txt").await;
        assert!(diff.contains("new file mode 100644"), "{diff}");
        assert!(diff.contains("@@ -0,0 +1,2 @@"), "{diff}");
        assert!(diff.contains("+line1\n+line2"), "{diff}");

        // Untracked binary file → placeholder.
        std::fs::write(dir.path().join("bin.dat"), [0u8, 159, 146, 150]).unwrap();
        assert_eq!(git_file_diff(&cwd, "bin.dat").await, "(binary file)");

        // Missing file → empty.
        assert_eq!(git_file_diff(&cwd, "missing.txt").await, "");
    }

    #[tokio::test]
    async fn file_diff_works_without_commits() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = path(&dir);
        StdCommand::new("git")
            .args(["init", "-b", "main"])
            .current_dir(&cwd)
            .output()
            .unwrap();
        std::fs::write(dir.path().join("staged.txt"), "staged content\n").unwrap();
        StdCommand::new("git")
            .args(["add", "staged.txt"])
            .current_dir(&cwd)
            .output()
            .unwrap();
        let diff = git_file_diff(&cwd, "staged.txt").await;
        assert!(diff.contains("staged content"), "{diff}");
    }

    #[tokio::test]
    async fn diff_for_commit_message_shapes() {
        let dir = init_repo();
        let cwd = path(&dir);
        // Clean repo → empty.
        assert_eq!(git_diff_for_commit_message(&cwd, None).await, "");
        // Explicit empty selection → empty.
        assert_eq!(git_diff_for_commit_message(&cwd, Some(&[])).await, "");

        // Since v1.2.11 this returns the real diff (via commit_message.rs),
        // not a status/stat summary.
        std::fs::write(dir.path().join("a.txt"), "hello\nworld\n").unwrap();
        let diff = git_diff_for_commit_message(&cwd, None).await;
        assert!(diff.contains("diff --git a/a.txt b/a.txt"), "{diff}");
        assert!(diff.contains("+world"), "{diff}");

        // Untracked files are picked up as synthesized all-added diffs.
        std::fs::write(dir.path().join("new.txt"), "brand new\n").unwrap();
        let diff = git_diff_for_commit_message(&cwd, None).await;
        assert!(diff.contains("new file mode 100644"), "{diff}");
        assert!(diff.contains("+brand new"), "{diff}");

        // Explicit path selection limits the diff to those files.
        let selected = git_diff_for_commit_message(&cwd, Some(&["new.txt".to_string()])).await;
        assert!(selected.contains("+brand new"), "{selected}");
        assert!(!selected.contains("+world"), "{selected}");

        // Path filter excluding every change → empty.
        let filtered = git_diff_for_commit_message(&cwd, Some(&["other.txt".to_string()])).await;
        assert_eq!(filtered, "");
    }
}
