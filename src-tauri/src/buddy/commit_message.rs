//! Commit-message generation — Rust port of
//! `src/main/buddy/commit-message.ts` (added upstream in v1.2.11).
//!
//! Collects the real diff for the selected files (staged + unstaged vs HEAD,
//! synthesized all-added diffs for untracked files), builds the Chinese
//! commit-message prompt verbatim, runs the resolved actor launcher once
//! (piped stdio or PTY, 120s timeout, cancellable via a process-wide slot),
//! and parses the actor's final output as a `{"type":"commit_message",...}`
//! JSON payload with a Conventional-Commits plain-text fallback.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;
use regex::Regex;
use serde::{Deserialize, Serialize};
use tokio::process::Command;

use super::defaults::{normalize_global_settings, normalize_launcher};
use super::launchers::{
    build_launcher_command, kind_needs_pty, parser_actor_for_kind, run_launcher,
    run_launcher_with_pty, LauncherCommandInput, LauncherCommandKind, LauncherError, PtyRunInput,
    RunLauncherInput,
};
use super::parsers::extract_actor_output;
use super::types::{GlobalSettings, Launcher, TaskSettings};

pub const COMMIT_MESSAGE_TIMEOUT_MS: u64 = 120_000;
pub const MAX_DIFF_BYTES: usize = 200_000;

pub const SUPPORTED_ACTORS: [&str; 5] = ["claude", "codex", "cursor", "opencode", "kimi"];

pub fn is_supported_actor(actor: &str) -> bool {
    SUPPORTED_ACTORS.contains(&actor)
}

// ---------------------------------------------------------------------------
// Diff collection — port of `gitDiffForSelectedFiles`
// ---------------------------------------------------------------------------

/// Port of the local `execGit` in commit-message.ts: resolves with the raw
/// (untrimmed) stdout, rejects with stderr or a synthetic exit message.
async fn exec_git(args: &[&str], cwd: &str) -> Result<String, String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .await
        .map_err(|error| error.to_string())?;
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let code = output
            .status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "null".to_string());
        return Err(if stderr.is_empty() {
            format!("git {} exited with {}", args.join(" "), code)
        } else {
            stderr
        });
    }
    Ok(stdout)
}

/// Port of `buildNewFileDiff`: synthesized all-added unified diff for an
/// untracked file.
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

#[derive(Debug, Clone, Default)]
pub struct SelectedDiffResult {
    pub paths: Vec<String>,
    pub diff: String,
    pub truncated: bool,
    pub total_bytes: usize,
}

/// JS `string.length`/`slice` operate on UTF-16 code units; we count/cut on
/// char boundaries instead (only differs for astral-plane characters) — same
/// trade-off `git.rs` already documents.
fn char_len(text: &str) -> usize {
    text.chars().count()
}

fn truncate_chars(text: &str, max: usize) -> String {
    if char_len(text) <= max {
        return text.to_string();
    }
    text.chars().take(max).collect()
}

/// Collect actual diffs for the selected file paths (staged + unstaged vs
/// HEAD). Untracked files get a synthesized all-added diff. Deleted files get
/// the deletion diff. Binary files are noted with a placeholder. Diffs are
/// truncated at [`MAX_DIFF_BYTES`] with a clear marker.
pub async fn git_diff_for_selected_files(cwd: &str, paths: &[String]) -> SelectedDiffResult {
    if paths.is_empty() {
        return SelectedDiffResult::default();
    }

    let mut diffs: Vec<String> = Vec::new();
    let mut total_bytes = 0usize;
    let mut truncated = false;

    for file_path in paths {
        if total_bytes >= MAX_DIFF_BYTES {
            truncated = true;
            break;
        }

        let diff = match exec_git(&["diff", "HEAD", "--no-renames", "--", file_path], cwd).await {
            Ok(diff) => diff,
            Err(_) => {
                // HEAD may not exist yet (no commits); staged + unstaged separately.
                let staged_args = ["diff", "--cached", "--no-renames", "--", file_path];
                let unstaged_args = ["diff", "--no-renames", "--", file_path];
                let (staged, unstaged) = tokio::join!(
                    exec_git(&staged_args, cwd),
                    exec_git(&unstaged_args, cwd),
                );
                [staged.unwrap_or_default(), unstaged.unwrap_or_default()]
                    .into_iter()
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        };

        if !diff.is_empty() {
            let remaining = MAX_DIFF_BYTES - total_bytes;
            if char_len(&diff) > remaining {
                diffs.push(format!("{}\n... (diff truncated)", truncate_chars(&diff, remaining)));
                truncated = true;
                break;
            }
            total_bytes += char_len(&diff);
            diffs.push(diff);
            continue;
        }

        // Untracked or new file: synthesize an all-added diff from disk content.
        let abs = Path::new(cwd).join(file_path);
        let is_file = std::fs::metadata(&abs).map(|m| m.is_file()).unwrap_or(false);
        if !is_file {
            // Deleted (or otherwise missing) file: try the deletion diff.
            if let Ok(del_diff) =
                exec_git(&["diff", "--no-renames", "--", file_path], cwd).await
            {
                if !del_diff.is_empty() {
                    let remaining = MAX_DIFF_BYTES - total_bytes;
                    if char_len(&del_diff) > remaining {
                        diffs.push(format!(
                            "{}\n... (diff truncated)",
                            truncate_chars(&del_diff, remaining)
                        ));
                        truncated = true;
                        break;
                    }
                    total_bytes += char_len(&del_diff);
                    diffs.push(del_diff);
                }
            }
            continue;
        }
        let Ok(buf) = std::fs::read(&abs) else {
            continue; // skip inaccessible files
        };
        if buf.contains(&0) {
            diffs.push(format!("Binary file {file_path} changed (binary content not shown)"));
            continue;
        }
        let content = String::from_utf8_lossy(&buf).to_string();
        let remaining = MAX_DIFF_BYTES - total_bytes;
        if char_len(&content) > remaining {
            let content = truncate_chars(&content, remaining);
            diffs.push(format!(
                "{}\n... (file content truncated)",
                build_new_file_diff(file_path, &content)
            ));
            truncated = true;
            break;
        }
        total_bytes += char_len(&content);
        diffs.push(build_new_file_diff(file_path, &content));
    }

    let diff = diffs.join("\n\n");
    let total_bytes = diff.len(); // TS: Buffer.byteLength(diffStr, 'utf8')
    SelectedDiffResult {
        paths: paths.to_vec(),
        diff,
        truncated,
        total_bytes,
    }
}

// ---------------------------------------------------------------------------
// Prompt builder — port of `buildCommitMessagePrompt` (text is verbatim)
// ---------------------------------------------------------------------------

fn lang_instruction(lang: Option<&str>) -> String {
    match lang {
        None | Some("") | Some("en") => "Write the commit message in English.".to_string(),
        Some("zh-CN") => "使用简体中文撰写提交信息。".to_string(),
        Some("zh-TW") => "使用繁體中文撰寫提交訊息。".to_string(),
        Some(other) => format!("Write the commit message in {other}."),
    }
}

pub struct CommitMessagePromptInput<'a> {
    pub paths: &'a [String],
    pub diff: &'a str,
    pub truncated: bool,
    pub lang: Option<&'a str>,
}

pub fn build_commit_message_prompt(input: &CommitMessagePromptInput) -> String {
    let paths_list = input
        .paths
        .iter()
        .map(|p| format!("- {p}"))
        .collect::<Vec<_>>()
        .join("\n");
    let truncation_note = if input.truncated {
        "\n\n注意: SELECTED_DIFF 已被截断。你可以通过工具读取 SELECTED_PATHS 中的文件来补充理解,但不得查看未列出的文件。"
    } else {
        ""
    };
    let diff = if input.diff.is_empty() {
        "(无 diff 内容)"
    } else {
        input.diff
    };
    let lang_instruction = lang_instruction(input.lang);

    format!(
        r#"你当前的任务是为一次 Git 提交生成提交信息。

## 变更范围

以下文件路径定义了本次提交的范围边界。提交信息只能总结这些文件的变化,不得描述未列出文件中的变化。

SELECTED_PATHS:
{paths_list}

## 实际 diff

以下是这些文件相对 HEAD 的实际 staged + unstaged diff:

SELECTED_DIFF:
{diff}
{truncation_note}

## 允许的操作

- 你可以调用工具读取代码文件、查看 Git 历史和理解项目上下文,用于理解名称、关系、行为和提交风格。
- 你可以读取 SELECTED_PATHS 中的文件以补充对变更的理解。

## 禁止的操作

- 不得修改、创建或删除任何文件。
- 不得执行 git add、git commit、git push、git reset 等写操作。
- 不得描述未选择文件中的变化。
- 不得在提交信息中添加 Co-Authored-By 等元数据。

## 输出格式要求

- 使用 Conventional Commits 格式。
- 标题格式为 type(scope): description,scope 可省略。
- 中文标题使用简洁的动作描述,不强制套用英文祈使语法。
- 第一行不超过 72 个字符。
- 非简单修改应包含正文。
- 正文可以包含项目符号(- 开头)、缩进续行和独立补充段落。
- 不添加 Co-Authored-By 等元数据。
- 最终只返回提交信息结果,不返回分析、思考过程、工具调用或解释。

{lang_instruction}

## 输出协议

最终返回以下 JSON 格式(仅返回此 JSON,不附加其他内容):

{{"type":"commit_message","message":"完整的提交信息"}}

其中 message 是任意合法的多行字符串,可以包含:
- Conventional Commit 标题
- 标题后的空行
- - 开头的项目符号
- 项目符号的缩进续行
- 多个正文段落
- 最后的补充说明

JSON 中换行通过 \n 表达。"#
    )
}

// ---------------------------------------------------------------------------
// Output parsing — port of `parseCommitMessageOutput`
// ---------------------------------------------------------------------------

fn conventional_commit_re() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^(feat|fix|chore|docs|style|refactor|perf|test|build|ci|revert)(\([^)]*\))?!?:\s.+")
            .unwrap()
    })
}

/// TS: `/\{[\s\S]*"type"\s*:\s*"commit_message"[\s\S]*\}/` (greedy).
fn commit_message_json_re() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?s)\{.*"type"\s*:\s*"commit_message".*\}"#).unwrap()
    })
}

const INVALID_MARKERS: [&str; 10] = [
    "<think>",
    "<tool_call>",
    "SEARCH/REPLACE",
    "```",
    "<tool_use>",
    "<function_calls>",
    "I'll examine",
    "I will examine",
    "Let me analyze",
    "Based on the diff",
];

fn has_invalid_markers(text: &str) -> bool {
    INVALID_MARKERS.iter().any(|m| text.contains(m))
}

fn normalize_newlines(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n").trim().to_string()
}

/// Parse the actor's final output to extract the commit message.
/// 1. Use the existing actor parser to extract final assistant/result output.
/// 2. Try JSON with `type = commit_message` and a `message` field.
/// 3. Fallback: accept plain text only if the entire output is a valid
///    Conventional Commit.
/// 4. Validate: reject outputs with think tags, tool calls, code fences, etc.
pub fn parse_commit_message_output(
    actor: &str,
    kind: LauncherCommandKind,
    raw_events: &str,
) -> Option<String> {
    let parser_actor = parser_actor_for_kind(actor, kind);
    let final_text = extract_actor_output(&parser_actor, raw_events).trim().to_string();

    if final_text.is_empty() {
        return None;
    }

    // Try JSON parse first.
    if let Some(json_match) = commit_message_json_re().find(&final_text) {
        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(json_match.as_str()) {
            let message = parsed
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("");
            if parsed.get("type").and_then(|t| t.as_str()) == Some("commit_message") {
                let message = message.trim();
                let first_line = message.split('\n').next().unwrap_or("");
                if !message.is_empty() && conventional_commit_re().is_match(first_line) {
                    if has_invalid_markers(message) {
                        return None;
                    }
                    return Some(normalize_newlines(message));
                }
            }
        }
        // fall through to plain text
    }

    // Fallback: accept plain text only if it looks like a valid Conventional Commit.
    let first_line = final_text.split('\n').next().unwrap_or("").trim();
    if !has_invalid_markers(&final_text)
        && conventional_commit_re().is_match(first_line)
        && !final_text.starts_with('{')
    {
        return Some(normalize_newlines(&final_text));
    }

    None
}

// ---------------------------------------------------------------------------
// Generation orchestration — port of `generateCommitMessageWithActor`
// ---------------------------------------------------------------------------

/// Service-level input (also the `buddy_generate_commit_message` command
/// payload; fields arrive camelCase from the frontend).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerateCommitMessageInput {
    pub repo_root: String,
    pub actor: String,
    #[serde(default)]
    pub lang: Option<String>,
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub task_settings: Option<TaskSettings>,
}

/// Port of the TS `GenerateCommitMessageInput` after launcher resolution.
pub struct GenerateCommitMessageActorInput {
    pub repo_root: String,
    pub actor: String,
    pub lang: Option<String>,
    pub paths: Vec<String>,
    pub launcher: Launcher,
}

#[derive(Debug)]
pub struct GenerateCommitMessageResult {
    pub message: String,
    pub log: CommitMessageLog,
}

/// Port of `CommitMessageLog`. Serialized to stderr as one JSON line; the log
/// records metadata only — never the diff, message, or secrets.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommitMessageLog {
    pub actor: String,
    pub launcher_kind: LauncherCommandKind,
    pub file_count: usize,
    pub diff_bytes: usize,
    pub diff_truncated: bool,
    pub start_time: String,
    pub end_time: String,
    pub duration_ms: u128,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub cancelled: bool,
    pub valid: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum CommitMessageError {
    /// `CommitMessageCancelledError`
    #[error("Commit message generation was cancelled")]
    Cancelled { log: CommitMessageLog },
    /// `CommitMessageTimeoutError`
    #[error("Commit message generation timed out")]
    Timeout { log: CommitMessageLog },
    /// `CommitMessageProcessError`
    #[error("Actor exited with code {exit_code}")]
    Process {
        log: CommitMessageLog,
        exit_code: i32,
        stderr: String,
    },
    /// `CommitMessageInvalidOutputError`
    #[error("Commit message output was invalid")]
    InvalidOutput { log: CommitMessageLog },
    /// Launcher-level failure (e.g. command not found); TS rethrows the raw
    /// `runLauncher` error without log metadata.
    #[error("{0}")]
    Launcher(#[from] LauncherError),
}

/// Handle for the in-flight commit-message generation. Only one generation
/// runs at a time (the commit dialog is single-instance), so a process-wide
/// slot is enough — same as the TS `activeController`.
struct ActiveGeneration {
    token: Arc<()>,
    abort: Arc<AtomicBool>,
}

static ACTIVE_GENERATION: Mutex<Option<ActiveGeneration>> = Mutex::new(None);

/// Cancel the active commit-message generation, if any (port of
/// `cancelGenerateCommitMessage`). Setting the flag makes
/// `run_launcher`/`run_launcher_with_pty` SIGTERM the child.
pub fn cancel_generate_commit_message() {
    let active = ACTIVE_GENERATION.lock().take();
    if let Some(active) = active {
        active.abort.store(true, Ordering::SeqCst);
    }
}

/// Test seam: lets tests shrink the 120s generation timeout.
#[cfg(test)]
static TEST_TIMEOUT_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn commit_message_timeout_ms() -> u64 {
    #[cfg(test)]
    {
        let override_ms = TEST_TIMEOUT_MS.load(Ordering::SeqCst);
        if override_ms > 0 {
            return override_ms;
        }
    }
    COMMIT_MESSAGE_TIMEOUT_MS
}

fn iso_now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

pub async fn generate_commit_message_with_actor(
    input: &GenerateCommitMessageActorInput,
) -> Result<GenerateCommitMessageResult, CommitMessageError> {
    let start = std::time::Instant::now();
    let start_time = iso_now();

    // TS cancels any previous generation before starting a new one.
    cancel_generate_commit_message();

    let abort = Arc::new(AtomicBool::new(false));
    let token = Arc::new(());
    *ACTIVE_GENERATION.lock() = Some(ActiveGeneration {
        token: token.clone(),
        abort: abort.clone(),
    });

    let diff_result = git_diff_for_selected_files(&input.repo_root, &input.paths).await;
    let prompt_text = build_commit_message_prompt(&CommitMessagePromptInput {
        paths: &diff_result.paths,
        diff: &diff_result.diff,
        truncated: diff_result.truncated,
        lang: input.lang.as_deref(),
    });

    // mkdtemp(join(tmpdir(), 'buddy-commit-'))
    let temp_dir = std::env::temp_dir().join(format!(
        "buddy-commit-{}",
        uuid::Uuid::new_v4().simple()
    ));
    tokio::fs::create_dir_all(&temp_dir)
        .await
        .map_err(|error| CommitMessageError::Launcher(LauncherError::Io(error)))?;
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default();
    let run_id = format!("commit_{millis}");
    let temp_dir_string = temp_dir.to_string_lossy().to_string();
    let event_file = format!("{temp_dir_string}/{run_id}-events.jsonl");
    let output_file = format!("{temp_dir_string}/{run_id}-output.md");
    let prompt_file = format!("{temp_dir_string}/{run_id}-prompt.md");
    if let Err(error) = tokio::fs::write(&prompt_file, &prompt_text).await {
        let _ = tokio::fs::remove_dir_all(&temp_dir).await;
        return Err(CommitMessageError::Launcher(LauncherError::Io(error)));
    }

    let launcher_command = build_launcher_command(&LauncherCommandInput {
        actor: input.actor.clone(),
        command: input.launcher.command.clone(),
        mode: Some("start".to_string()),
        prompt_file: prompt_file.clone(),
        prompt_text: Some(prompt_text),
        event_file: Some(event_file),
        output_file: Some(output_file),
        repo_root: Some(input.repo_root.clone()),
        ..Default::default()
    });

    let mut output_lines: Vec<String> = Vec::new();
    let mut stderr_lines: Vec<String> = Vec::new();

    // TS: `{ ...launcher.env, ...(launcherCommand.env ?? {}) }`.
    let mut merged_env = input.launcher.env.clone();
    if let Some(launcher_env) = &launcher_command.env {
        merged_env.extend(launcher_env.clone());
    }

    let timeout_ms = commit_message_timeout_ms();
    let run_result = if kind_needs_pty(launcher_command.kind) {
        run_launcher_with_pty(
            &PtyRunInput {
                command: launcher_command.command.clone(),
                args: launcher_command.args.clone(),
                cwd: input.repo_root.clone(),
                env: Some(merged_env),
                timeout_ms,
                abort: Some(abort.clone()),
            },
            |data| {
                for line in data.split('\n') {
                    let line = line.strip_suffix('\r').unwrap_or(line);
                    if !line.is_empty() {
                        output_lines.push(line.to_string());
                    }
                }
            },
        )
        .await
    } else {
        run_launcher(
            &RunLauncherInput {
                command: launcher_command.command.clone(),
                args: launcher_command.args.clone(),
                cwd: input.repo_root.clone(),
                env: Some(merged_env),
                stdin_text: launcher_command.stdin_text.clone(),
                timeout_ms,
                abort: Some(abort.clone()),
            },
            |line| output_lines.push(line),
            |line| stderr_lines.push(line),
        )
        .await
    };

    let mut exit_code: Option<i32> = None;
    let mut exit_signal: Option<String> = None;
    match run_result {
        Ok(result) => {
            exit_code = result.exit_code;
            exit_signal = result.signal;
        }
        Err(error) => {
            // TS rethrows non-abort run errors as-is (skipping the metadata
            // log); we still clean up the temp dir.
            if !abort.load(Ordering::SeqCst) {
                let _ = tokio::fs::remove_dir_all(&temp_dir).await;
                let mut guard = ACTIVE_GENERATION.lock();
                if matches!(guard.as_ref(), Some(a) if Arc::ptr_eq(&a.token, &token)) {
                    *guard = None;
                }
                drop(guard);
                return Err(CommitMessageError::Launcher(error));
            }
        }
    }

    let end_time = iso_now();
    let duration_ms = start.elapsed().as_millis();

    // run_launcher/run_launcher_with_pty resolve normally after an abort, so
    // the abort flag must be checked explicitly. A SIGTERM/15 termination with
    // a null exit code and no abort is the timeout kill.
    let cancelled = abort.load(Ordering::SeqCst);
    let timed_out = !cancelled
        && matches!(exit_signal.as_deref(), Some("SIGTERM") | Some("15"))
        && exit_code.is_none();

    let stdout_text = output_lines.join("\n");
    let mut message = String::new();
    let mut valid = false;
    if !cancelled && !timed_out && exit_code == Some(0) {
        message =
            parse_commit_message_output(&input.actor, launcher_command.kind, &stdout_text)
                .unwrap_or_default();
        valid = !message.is_empty();
    }

    let log = CommitMessageLog {
        actor: input.actor.clone(),
        launcher_kind: launcher_command.kind,
        file_count: input.paths.len(),
        diff_bytes: diff_result.total_bytes,
        diff_truncated: diff_result.truncated,
        start_time,
        end_time,
        duration_ms,
        exit_code,
        timed_out,
        cancelled,
        valid,
    };

    // Metadata only — never the diff, the message, or secrets.
    eprintln!(
        "[commit-message] {}",
        serde_json::to_string(&log).unwrap_or_default()
    );

    let _ = tokio::fs::remove_dir_all(&temp_dir).await;
    let mut guard = ACTIVE_GENERATION.lock();
    if matches!(guard.as_ref(), Some(a) if Arc::ptr_eq(&a.token, &token)) {
        *guard = None;
    }
    drop(guard);

    if cancelled {
        return Err(CommitMessageError::Cancelled { log });
    }
    if timed_out {
        return Err(CommitMessageError::Timeout { log });
    }
    if let Some(code) = exit_code {
        if code != 0 {
            return Err(CommitMessageError::Process {
                log,
                exit_code: code,
                stderr: stderr_lines.join("\n").trim().to_string(),
            });
        }
    }
    if !valid || message.is_empty() {
        return Err(CommitMessageError::InvalidOutput { log });
    }

    Ok(GenerateCommitMessageResult { message, log })
}

// ---------------------------------------------------------------------------
// Actor / launcher resolution — port of `resolveCommitMessageActor` and
// `resolveLauncher`
// ---------------------------------------------------------------------------

pub fn resolve_commit_message_actor(
    stored_actor: Option<&str>,
    task_implementer: Option<&str>,
) -> &'static str {
    if let Some(actor) = stored_actor {
        if is_supported_actor(actor) {
            return SUPPORTED_ACTORS
                .into_iter()
                .find(|a| *a == actor)
                .unwrap_or("claude");
        }
    }
    if let Some(actor) = task_implementer {
        if is_supported_actor(actor) {
            return SUPPORTED_ACTORS
                .into_iter()
                .find(|a| *a == actor)
                .unwrap_or("claude");
        }
    }
    "claude"
}

/// Task launchers → global launchers → per-actor default.
pub fn resolve_launcher(
    actor: &str,
    task_settings: Option<&TaskSettings>,
    global_settings: Option<&GlobalSettings>,
) -> Launcher {
    let task_launcher = task_settings.and_then(|s| s.launchers.get(actor));
    if let Some(launcher) = task_launcher {
        // TS truthiness check on `taskLauncher.command`.
        if !launcher.command.is_empty() {
            return normalize_launcher(actor, Some(launcher));
        }
    }
    let global_settings = normalize_global_settings(global_settings);
    if let Some(launcher) = global_settings
        .launchers
        .as_ref()
        .and_then(|launchers| launchers.get(actor))
    {
        if !launcher.command.is_empty() {
            return launcher.clone();
        }
    }
    normalize_launcher(actor, None)
}

// ---------------------------------------------------------------------------
// Tests — port of tests/unit/main/buddy-commit-message.test.ts plus the
// git/upstream diff coverage from buddy-git.test.ts
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::process::Command as StdCommand;
    use std::time::Duration;

    /// Serializes tests that exercise the process-global `ACTIVE_GENERATION`
    /// slot / timeout override, so parallel cargo tests can't cancel or
    /// retime each other's generations.
    static GENERATE_TEST_LOCK: Mutex<()> = Mutex::new(());

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

    /// Write an executable shell script into `dir` and return its path.
    fn write_script(dir: &std::path::Path, name: &str, body: &str) -> String {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path.to_string_lossy().to_string()
    }

    fn launcher_for(command: &str) -> Launcher {
        Launcher {
            command: command.to_string(),
            env: HashMap::new(),
            timeout_seconds: 120,
        }
    }

    fn actor_input(repo_root: &str, actor: &str, command: &str, paths: &[&str]) -> GenerateCommitMessageActorInput {
        GenerateCommitMessageActorInput {
            repo_root: repo_root.to_string(),
            actor: actor.to_string(),
            lang: None,
            paths: paths.iter().map(|p| p.to_string()).collect(),
            launcher: launcher_for(command),
        }
    }

    fn task_settings_with_launcher(actor: &str, command: &str) -> TaskSettings {
        let mut launchers = HashMap::new();
        launchers.insert(
            actor.to_string(),
            Launcher {
                command: command.to_string(),
                env: HashMap::new(),
                timeout_seconds: 100,
            },
        );
        TaskSettings {
            protocol_version: "1".to_string(),
            flow_policy: "dual".to_string(),
            role_mode: "dual".to_string(),
            launchers,
            implementer_actor: None,
            reviewer_actor: None,
            max_consecutive_failures: None,
            seed_claude_session_id: None,
            seed_codex_thread_id: None,
            seed_cursor_session_id: None,
            seed_opencode_session_id: None,
            seed_kimi_session_id: None,
        }
    }

    // -----------------------------------------------------------------------
    // isSupportedActor / resolveCommitMessageActor / resolveLauncher
    // -----------------------------------------------------------------------

    #[test]
    fn supported_actors_cover_the_five_and_reject_others() {
        for actor in ["claude", "codex", "cursor", "opencode", "kimi"] {
            assert!(is_supported_actor(actor), "{actor}");
        }
        for actor in ["human", "", "chatgpt"] {
            assert!(!is_supported_actor(actor), "{actor}");
        }
    }

    #[test]
    fn resolve_actor_prefers_stored_then_implementer_then_claude() {
        assert_eq!(resolve_commit_message_actor(Some("codex"), Some("claude")), "codex");
        assert_eq!(resolve_commit_message_actor(Some("invalid"), Some("cursor")), "cursor");
        assert_eq!(resolve_commit_message_actor(None, Some("invalid")), "claude");
        assert_eq!(resolve_commit_message_actor(None, None), "claude");
    }

    #[test]
    fn resolve_launcher_prefers_task_then_global_then_default() {
        let task = task_settings_with_launcher("codex", "codex --profile test");
        let launcher = resolve_launcher("codex", Some(&task), None);
        assert_eq!(launcher.command, "codex --profile test");

        let mut global_launchers = HashMap::new();
        global_launchers.insert(
            "claude".to_string(),
            Launcher {
                command: "claude --global".to_string(),
                env: HashMap::new(),
                timeout_seconds: 7200,
            },
        );
        let global = GlobalSettings {
            launchers: Some(global_launchers),
            ..Default::default()
        };
        let launcher = resolve_launcher("claude", None, Some(&global));
        assert_eq!(launcher.command, "claude --global");

        let launcher = resolve_launcher("kimi", None, None);
        assert_eq!(launcher.command, "kimi");
    }

    // -----------------------------------------------------------------------
    // buildCommitMessagePrompt
    // -----------------------------------------------------------------------

    fn prompt(paths: &[&str], diff: &str, truncated: bool, lang: Option<&str>) -> String {
        let paths: Vec<String> = paths.iter().map(|p| p.to_string()).collect();
        build_commit_message_prompt(&CommitMessagePromptInput {
            paths: &paths,
            diff,
            truncated,
            lang,
        })
    }

    #[test]
    fn prompt_includes_selected_paths_and_diff() {
        let prompt = prompt(
            &["src/app.ts", "src/util.ts"],
            "diff --git a/src/app.ts b/src/app.ts\n+new code",
            false,
            Some("zh-CN"),
        );
        assert!(prompt.contains("SELECTED_PATHS:"));
        assert!(prompt.contains("- src/app.ts"));
        assert!(prompt.contains("- src/util.ts"));
        assert!(prompt.contains("SELECTED_DIFF:"));
        assert!(prompt.contains("diff --git a/src/app.ts b/src/app.ts"));
        assert!(prompt.contains("使用简体中文撰写提交信息"));
        assert!(prompt.contains("commit_message"));
    }

    #[test]
    fn prompt_includes_truncation_note_when_truncated() {
        let prompt = prompt(&["src/app.ts"], "truncated diff content", true, Some("en"));
        assert!(prompt.contains("SELECTED_DIFF 已被截断"));
        assert!(prompt.contains("Write the commit message in English"));
    }

    #[test]
    fn prompt_forbids_writes_and_git_mutations() {
        let prompt = prompt(&["src/app.ts"], "some diff", false, None);
        assert!(prompt.contains("不得修改、创建或删除任何文件"));
        assert!(prompt.contains("不得执行 git add、git commit、git push、git reset"));
        assert!(prompt.contains("不得描述未选择文件中的变化"));
    }

    #[test]
    fn prompt_language_instruction_variants() {
        assert!(prompt(&["a"], "d", false, Some("zh-TW")).contains("使用繁體中文撰寫提交訊息"));
        // TS: any other language is named explicitly.
        assert!(prompt(&["a"], "d", false, Some("fr")).contains("Write the commit message in fr."));
        assert!(prompt(&["a"], "d", false, None).contains("Write the commit message in English."));
    }

    // -----------------------------------------------------------------------
    // parseCommitMessageOutput
    // -----------------------------------------------------------------------

    const KIND: LauncherCommandKind = LauncherCommandKind::NativeCodex;

    fn codex_event(text: &str) -> String {
        // Mirror the TS test fixture: a single `completed` event carrying the
        // text as an output_text content part.
        serde_json::json!({
            "type": "completed",
            "content": [{ "type": "output_text", "text": text }],
        })
        .to_string()
    }

    #[test]
    fn parses_valid_json_commit_message_output() {
        let raw = codex_event("{\"type\":\"commit_message\",\"message\":\"feat: add new feature\\n\\n- Added X\\n- Updated Y\"}");
        let result = parse_commit_message_output("codex", KIND, &raw);
        assert_eq!(result.as_deref(), Some("feat: add new feature\n\n- Added X\n- Updated Y"));
    }

    #[test]
    fn preserves_multiline_message_with_bullets_and_indentation() {
        let raw = codex_event("{\"type\":\"commit_message\",\"message\":\"docs: 新增可选安装渠道\\n\\n- 4 个 README 各新增小节，\\n  提供安装命令\\n- 新增维护文档\\n\\n对应 Tap 仓库。\"}");
        let result = parse_commit_message_output("codex", KIND, &raw).expect("parsed");
        assert!(result.contains("docs: 新增可选安装渠道"));
        assert!(result.contains("\n\n"));
        assert!(result.contains("- 4 个 README 各新增小节，"));
        assert!(result.contains("  提供安装命令"));
        assert!(result.contains("- 新增维护文档"));
        assert!(result.contains("对应 Tap 仓库。"));
    }

    #[test]
    fn returns_none_for_empty_output() {
        assert_eq!(parse_commit_message_output("codex", KIND, ""), None);
    }

    #[test]
    fn returns_none_for_think_tags() {
        let raw = codex_event("<think>Let me analyze</think>\nfeat: something");
        assert_eq!(parse_commit_message_output("codex", KIND, &raw), None);
    }

    #[test]
    fn returns_none_for_code_fences() {
        let raw = codex_event("```\nfeat: something\n```");
        assert_eq!(parse_commit_message_output("codex", KIND, &raw), None);
    }

    #[test]
    fn returns_none_for_tool_call_markers() {
        let raw = codex_event("<tool_call>read_file</tool_call>\nfeat: something");
        assert_eq!(parse_commit_message_output("codex", KIND, &raw), None);
    }

    #[test]
    fn returns_none_for_non_conventional_json_message() {
        let raw = codex_event("{\"type\":\"commit_message\",\"message\":\"just some text without type\"}");
        assert_eq!(parse_commit_message_output("codex", KIND, &raw), None);
    }

    #[test]
    fn falls_back_to_plain_text_conventional_commit() {
        let raw = codex_event("fix(api): handle null response\\n\\n- Added null check\\n- Returns empty array");
        let result = parse_commit_message_output("codex", KIND, &raw).expect("parsed");
        assert!(result.contains("fix(api): handle null response"));
        assert!(result.contains("- Added null check"));
    }

    #[test]
    fn accepts_various_conventional_title_formats() {
        for title in [
            "docs: description",
            "docs(readme): description",
            "feat!: description",
            "fix(api)!: description",
        ] {
            let raw = codex_event(title);
            assert!(
                parse_commit_message_output("codex", KIND, &raw).is_some(),
                "{title}"
            );
        }
    }

    #[test]
    fn plain_text_fallback_keeps_bullet_points() {
        let raw = codex_event("chore: cleanup\\n\\n- removed dead code\\n- fixed imports");
        let result = parse_commit_message_output("codex", KIND, &raw).expect("parsed");
        assert!(result.contains("- removed dead code"));
        assert!(result.contains("- fixed imports"));
    }

    // -----------------------------------------------------------------------
    // gitDiffForSelectedFiles
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn selected_diff_empty_for_empty_paths() {
        let result = git_diff_for_selected_files("/tmp/nonexistent", &[]).await;
        assert_eq!(result.diff, "");
        assert!(result.paths.is_empty());
        assert!(!result.truncated);
        assert_eq!(result.total_bytes, 0);
    }

    #[tokio::test]
    async fn selected_diff_returns_real_diff_for_modified_files() {
        let dir = init_repo();
        let cwd = path(&dir);
        std::fs::write(dir.path().join("a.txt"), "hello\nworld\n").unwrap();
        let result = git_diff_for_selected_files(&cwd, &["a.txt".to_string()]).await;
        assert!(result.diff.contains("diff --git a/a.txt b/a.txt"), "{}", result.diff);
        assert!(result.diff.contains("+world"), "{}", result.diff);
        assert!(!result.truncated);
    }

    #[tokio::test]
    async fn selected_diff_synthesizes_new_file_diff_for_untracked() {
        let dir = init_repo();
        let cwd = path(&dir);
        std::fs::write(dir.path().join("new-file.txt"), "new content\n").unwrap();
        let result = git_diff_for_selected_files(&cwd, &["new-file.txt".to_string()]).await;
        assert!(result.diff.contains("new file mode"), "{}", result.diff);
        assert!(result.diff.contains("+new content"), "{}", result.diff);
    }

    #[tokio::test]
    async fn selected_diff_excludes_unselected_files() {
        let dir = init_repo();
        let cwd = path(&dir);
        std::fs::write(dir.path().join("selected.txt"), "selected\n").unwrap();
        std::fs::write(dir.path().join("unselected.txt"), "unselected\n").unwrap();
        StdCommand::new("git")
            .args(["add", "-A"])
            .current_dir(&cwd)
            .output()
            .unwrap();
        StdCommand::new("git")
            .args(["commit", "-m", "base"])
            .current_dir(&cwd)
            .output()
            .unwrap();
        std::fs::write(dir.path().join("selected.txt"), "selected modified\n").unwrap();
        std::fs::write(dir.path().join("unselected.txt"), "unselected modified\n").unwrap();

        let result = git_diff_for_selected_files(&cwd, &["selected.txt".to_string()]).await;
        assert!(result.diff.contains("selected modified"), "{}", result.diff);
        assert!(!result.diff.contains("unselected modified"), "{}", result.diff);
    }

    #[tokio::test]
    async fn selected_diff_works_without_head() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = path(&dir);
        StdCommand::new("git")
            .args(["init", "-b", "main"])
            .current_dir(&cwd)
            .output()
            .unwrap();
        std::fs::write(dir.path().join("file.txt"), "content\n").unwrap();
        StdCommand::new("git")
            .args(["add", "file.txt"])
            .current_dir(&cwd)
            .output()
            .unwrap();
        let result = git_diff_for_selected_files(&cwd, &["file.txt".to_string()]).await;
        assert!(result.diff.contains("content"), "{}", result.diff);
    }

    #[tokio::test]
    async fn selected_diff_marks_binary_and_deleted_files() {
        let dir = init_repo();
        let cwd = path(&dir);
        // Untracked binary file → placeholder line.
        std::fs::write(dir.path().join("bin.dat"), [0u8, 159, 146, 150]).unwrap();
        let result = git_diff_for_selected_files(&cwd, &["bin.dat".to_string()]).await;
        assert_eq!(
            result.diff,
            "Binary file bin.dat changed (binary content not shown)"
        );
        // Deleted tracked file → deletion diff.
        std::fs::remove_file(dir.path().join("a.txt")).unwrap();
        let result = git_diff_for_selected_files(&cwd, &["a.txt".to_string()]).await;
        assert!(result.diff.contains("-hello"), "{}", result.diff);
    }

    // -----------------------------------------------------------------------
    // generateCommitMessageWithActor orchestration (fake shell-script actors)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn generates_message_via_fake_actor() {
        let _guard = GENERATE_TEST_LOCK.lock();
        let dir = init_repo();
        let cwd = path(&dir);
        std::fs::write(dir.path().join("a.txt"), "hello\nworld\n").unwrap();
        // Named `codex` so command_kind_for maps it to NativeCodex.
        let script = write_script(
            dir.path(),
            "codex",
            "#!/bin/sh\ncat >/dev/null\necho '{\"type\":\"completed\",\"content\":[{\"type\":\"output_text\",\"text\":\"{\\\"type\\\":\\\"commit_message\\\",\\\"message\\\":\\\"feat: generated commit message\\\"}\"}]}'\n",
        );
        let result = generate_commit_message_with_actor(&actor_input(&cwd, "codex", &script, &["a.txt"]))
            .await
            .unwrap();
        assert_eq!(result.message, "feat: generated commit message");
        assert!(result.log.valid);
        assert_eq!(result.log.exit_code, Some(0));
    }

    #[tokio::test]
    async fn process_error_on_nonzero_exit() {
        let _guard = GENERATE_TEST_LOCK.lock();
        let dir = init_repo();
        let cwd = path(&dir);
        std::fs::write(dir.path().join("a.txt"), "hello\nworld\n").unwrap();
        let script = write_script(
            dir.path(),
            "codex",
            "#!/bin/sh\ncat >/dev/null\necho 'boom' >&2\nexit 1\n",
        );
        let error = generate_commit_message_with_actor(&actor_input(&cwd, "codex", &script, &["a.txt"]))
            .await
            .unwrap_err();
        match error {
            CommitMessageError::Process { exit_code, stderr, .. } => {
                assert_eq!(exit_code, 1);
                assert!(stderr.contains("boom"), "{stderr}");
            }
            other => panic!("expected Process error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn invalid_output_error_on_empty_output() {
        let _guard = GENERATE_TEST_LOCK.lock();
        let dir = init_repo();
        let cwd = path(&dir);
        std::fs::write(dir.path().join("a.txt"), "hello\nworld\n").unwrap();
        let script = write_script(dir.path(), "codex", "#!/bin/sh\ncat >/dev/null\nexit 0\n");
        let error = generate_commit_message_with_actor(&actor_input(&cwd, "codex", &script, &["a.txt"]))
            .await
            .unwrap_err();
        assert!(matches!(error, CommitMessageError::InvalidOutput { .. }), "{error:?}");
    }

    #[tokio::test]
    async fn launcher_error_on_missing_command() {
        let _guard = GENERATE_TEST_LOCK.lock();
        let dir = init_repo();
        let cwd = path(&dir);
        std::fs::write(dir.path().join("a.txt"), "hello\nworld\n").unwrap();
        let error = generate_commit_message_with_actor(&actor_input(
            &cwd,
            "claude",
            "definitely-not-a-real-command-xyz",
            &["a.txt"],
        ))
        .await
        .unwrap_err();
        assert!(matches!(error, CommitMessageError::Launcher(_)), "{error:?}");
    }

    #[tokio::test]
    async fn timeout_error_when_actor_runs_past_deadline() {
        let _guard = GENERATE_TEST_LOCK.lock();
        TEST_TIMEOUT_MS.store(300, Ordering::SeqCst);
        let dir = init_repo();
        let cwd = path(&dir);
        std::fs::write(dir.path().join("a.txt"), "hello\nworld\n").unwrap();
        let script = write_script(dir.path(), "claude", "#!/bin/sh\nsleep 60\n");
        let started = std::time::Instant::now();
        let error = generate_commit_message_with_actor(&actor_input(&cwd, "claude", &script, &["a.txt"]))
            .await
            .unwrap_err();
        TEST_TIMEOUT_MS.store(0, Ordering::SeqCst);
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(matches!(error, CommitMessageError::Timeout { .. }), "{error:?}");
    }

    #[tokio::test]
    async fn pty_timeout_counts_as_timeout() {
        let _guard = GENERATE_TEST_LOCK.lock();
        TEST_TIMEOUT_MS.store(300, Ordering::SeqCst);
        let dir = init_repo();
        let cwd = path(&dir);
        std::fs::write(dir.path().join("a.txt"), "hello\nworld\n").unwrap();
        // Named `opencode` so the run goes through the PTY path, which
        // reports a timeout as exit_code None + signal "15".
        let script = write_script(dir.path(), "opencode", "#!/bin/sh\nsleep 60\n");
        let error = generate_commit_message_with_actor(&actor_input(&cwd, "opencode", &script, &["a.txt"]))
            .await
            .unwrap_err();
        TEST_TIMEOUT_MS.store(0, Ordering::SeqCst);
        assert!(matches!(error, CommitMessageError::Timeout { .. }), "{error:?}");
    }

    #[tokio::test]
    async fn cancel_interrupts_in_flight_generation() {
        let _guard = GENERATE_TEST_LOCK.lock();
        let dir = init_repo();
        let cwd = path(&dir);
        std::fs::write(dir.path().join("a.txt"), "hello\nworld\n").unwrap();
        let script = write_script(dir.path(), "claude", "#!/bin/sh\nsleep 60\n");
        let cwd_clone = cwd.clone();
        let script_clone = script.clone();
        let handle = tokio::spawn(async move {
            generate_commit_message_with_actor(&actor_input(&cwd_clone, "claude", &script_clone, &["a.txt"])).await
        });
        // Wait until the generation has actually registered in the
        // process-wide slot before cancelling — a fixed sleep races the
        // child-process spawn under parallel test load.
        let registered = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if ACTIVE_GENERATION.lock().is_some() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        assert!(registered.is_ok(), "generation did not start in time");
        let started = std::time::Instant::now();
        cancel_generate_commit_message();
        let error = tokio::time::timeout(Duration::from_secs(10), handle)
            .await
            .expect("generation did not finish after cancel")
            .unwrap()
            .unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(matches!(error, CommitMessageError::Cancelled { .. }), "{error:?}");
        // Cancelling with nothing in flight is a no-op.
        cancel_generate_commit_message();
    }

    // -----------------------------------------------------------------------
    // Command-payload serde (frontend contract)
    // -----------------------------------------------------------------------

    #[test]
    fn generate_input_deserializes_camel_case() {
        let input: GenerateCommitMessageInput = serde_json::from_value(serde_json::json!({
            "repoRoot": "/tmp/repo",
            "actor": "codex",
            "lang": "zh-CN",
            "paths": ["a.txt"],
        }))
        .unwrap();
        assert_eq!(input.repo_root, "/tmp/repo");
        assert_eq!(input.actor, "codex");
        assert_eq!(input.lang.as_deref(), Some("zh-CN"));
        assert_eq!(input.paths, vec!["a.txt".to_string()]);
        assert!(input.task_settings.is_none());
    }
}
