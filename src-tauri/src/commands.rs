//! Tauri command surface — port of `src/main/ipc/buddy-handlers.ts` plus the
//! extra handlers in `src/main/index.ts` of the Electron edition.
//!
//! Naming contract (the frontend bridge in `src/lib/tauri-bridge.ts` already
//! calls these): every `buddy:xxx` IPC channel becomes the snake_case command
//! `buddy_xxx`; JS passes arguments as a single camelCase object, which Tauri
//! maps onto the snake_case Rust parameters.

use std::collections::HashMap;

use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use serde::Serialize;
use tauri::{AppHandle, State};

use crate::buddy::commit_message::GenerateCommitMessageInput;
use crate::buddy::git::GitCommitResult;
use crate::buddy::service::{BuddyCoreService, EventsResponse};
use crate::buddy::types::{
    AttachmentMeta, BootstrapResponse, CountdownInput, CreateTaskInput, CreateTaskResult,
    GlobalSettings, GitStatusResult, InstructionQueueItem, RoundEventSummary, SendMessageInput,
    StartTaskInput, Task, TaskDetail, TaskStats, TestLauncherResult,
};
use crate::{menu, updater};

type CmdResult<T> = Result<T, String>;

fn err<E: std::fmt::Display>(error: E) -> String {
    error.to_string()
}

// ---------------------------------------------------------------------------
// buddy:* (port of registerBuddyHandlers)
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn buddy_check_health(service: State<'_, BuddyCoreService>) -> CmdResult<bool> {
    Ok(service.check_health().await)
}

#[tauri::command]
pub async fn buddy_bootstrap(
    service: State<'_, BuddyCoreService>,
) -> CmdResult<BootstrapResponse> {
    service.bootstrap().await.map_err(err)
}

#[tauri::command]
pub async fn buddy_get_tasks(service: State<'_, BuddyCoreService>) -> CmdResult<Vec<Task>> {
    Ok(service.get_tasks().await)
}

#[tauri::command]
pub async fn buddy_get_task_detail(
    service: State<'_, BuddyCoreService>,
    task_id: String,
    workspace_key: Option<String>,
) -> CmdResult<TaskDetail> {
    service
        .get_task_detail(&task_id, workspace_key.as_deref())
        .await
        .map_err(err)
}

#[tauri::command]
pub async fn buddy_create_task(
    service: State<'_, BuddyCoreService>,
    input: CreateTaskInput,
) -> CmdResult<CreateTaskResult> {
    service.create_task(input).await.map_err(err)
}

#[tauri::command]
pub async fn buddy_delete_task(
    service: State<'_, BuddyCoreService>,
    task_id: String,
    workspace_key: Option<String>,
) -> CmdResult<()> {
    service
        .delete_task(&task_id, workspace_key.as_deref())
        .await
        .map_err(err)
}

#[tauri::command]
pub async fn buddy_start_task(
    service: State<'_, BuddyCoreService>,
    task_id: String,
    input: StartTaskInput,
) -> CmdResult<()> {
    service.start_task(&task_id, input).await.map_err(err)
}

#[tauri::command]
pub async fn buddy_send_message(
    service: State<'_, BuddyCoreService>,
    task_id: String,
    input: SendMessageInput,
) -> CmdResult<()> {
    service.send_message(&task_id, input).await.map_err(err)
}

#[tauri::command]
pub async fn buddy_skip_countdown(
    service: State<'_, BuddyCoreService>,
    task_id: String,
    input: CountdownInput,
) -> CmdResult<()> {
    service.skip_countdown(&task_id, input).await.map_err(err)
}

#[tauri::command]
pub async fn buddy_pause_countdown(
    service: State<'_, BuddyCoreService>,
    task_id: String,
    input: CountdownInput,
) -> CmdResult<()> {
    service.pause_countdown(&task_id, input).await.map_err(err)
}

#[tauri::command]
pub async fn buddy_interrupt(
    service: State<'_, BuddyCoreService>,
    task_id: String,
    workspace_key: Option<String>,
) -> CmdResult<()> {
    service
        .interrupt(&task_id, workspace_key.as_deref())
        .await
        .map_err(err)
}

#[tauri::command]
pub async fn buddy_enqueue_instruction(
    service: State<'_, BuddyCoreService>,
    task_id: String,
    workspace_key: String,
    content: String,
    attachments: Option<Vec<AttachmentMeta>>,
) -> CmdResult<InstructionQueueItem> {
    service
        .enqueue_instruction(&task_id, &workspace_key, &content, attachments)
        .await
        .map_err(err)
}

#[tauri::command]
pub async fn buddy_dequeue_instruction(
    service: State<'_, BuddyCoreService>,
    task_id: String,
    workspace_key: String,
    item_id: String,
) -> CmdResult<()> {
    service
        .dequeue_instruction(&task_id, &workspace_key, &item_id)
        .await
        .map_err(err)
}

#[tauri::command]
pub async fn buddy_clear_instruction_queue(
    service: State<'_, BuddyCoreService>,
    task_id: String,
    workspace_key: String,
) -> CmdResult<()> {
    service
        .clear_instruction_queue(&task_id, &workspace_key)
        .await
        .map_err(err)
}

#[tauri::command]
pub async fn buddy_interrupt_and_insert(
    service: State<'_, BuddyCoreService>,
    task_id: String,
    workspace_key: String,
    queue_item_id: String,
) -> CmdResult<()> {
    service
        .interrupt_and_insert(&task_id, &workspace_key, &queue_item_id)
        .await
        .map_err(err)
}

#[tauri::command]
pub async fn buddy_get_events(
    service: State<'_, BuddyCoreService>,
    task_id: String,
    since: u64,
    workspace_key: Option<String>,
) -> CmdResult<EventsResponse> {
    service
        .get_events(&task_id, since, workspace_key.as_deref())
        .await
        .map_err(err)
}

#[tauri::command]
pub async fn buddy_get_round_events(
    service: State<'_, BuddyCoreService>,
    task_id: String,
    run_id: String,
    workspace_key: Option<String>,
    actor: Option<String>,
) -> CmdResult<Option<RoundEventSummary>> {
    service
        .get_round_events(
            &task_id,
            &run_id,
            workspace_key.as_deref(),
            actor.as_deref(),
            None,
        )
        .await
        .map_err(err)
}

#[tauri::command]
pub async fn buddy_get_task_stats(
    service: State<'_, BuddyCoreService>,
    task_id: String,
    workspace_key: Option<String>,
) -> CmdResult<Option<TaskStats>> {
    service
        .get_task_stats(&task_id, workspace_key.as_deref())
        .await
        .map_err(err)
}

#[tauri::command]
pub async fn buddy_update_global_settings(
    service: State<'_, BuddyCoreService>,
    settings: GlobalSettings,
) -> CmdResult<GlobalSettings> {
    service.update_global_settings(&settings).await.map_err(err)
}

#[tauri::command]
pub async fn buddy_git_status(
    service: State<'_, BuddyCoreService>,
    repo_root: String,
) -> CmdResult<GitStatusResult> {
    Ok(service.git_status(&repo_root).await)
}

#[tauri::command]
pub async fn buddy_git_stage_all(
    service: State<'_, BuddyCoreService>,
    repo_root: String,
) -> CmdResult<()> {
    service.git_stage_all(&repo_root).await.map_err(err)
}

#[tauri::command]
pub async fn buddy_git_stage_files(
    service: State<'_, BuddyCoreService>,
    repo_root: String,
    paths: Vec<String>,
) -> CmdResult<()> {
    service.git_stage_files(&repo_root, &paths).await.map_err(err)
}

#[tauri::command]
pub async fn buddy_git_commit_and_push(
    service: State<'_, BuddyCoreService>,
    repo_root: String,
    message: String,
    remote: String,
    push: Option<bool>,
) -> CmdResult<GitCommitResult> {
    service
        .git_commit_and_push(&repo_root, &message, &remote, push.unwrap_or(false))
        .await
        .map_err(err)
}

#[tauri::command]
pub async fn buddy_git_diff_for_commit_message(
    service: State<'_, BuddyCoreService>,
    repo_root: String,
    paths: Option<Vec<String>>,
) -> CmdResult<String> {
    Ok(service
        .git_diff_for_commit_message(&repo_root, paths.as_deref())
        .await)
}

#[tauri::command]
pub async fn buddy_git_file_diff(
    service: State<'_, BuddyCoreService>,
    repo_root: String,
    file_path: String,
) -> CmdResult<String> {
    Ok(service.git_file_diff(&repo_root, &file_path).await)
}

#[tauri::command]
pub async fn buddy_git_branches(
    service: State<'_, BuddyCoreService>,
    repo_root: String,
) -> CmdResult<Vec<String>> {
    Ok(service.git_branches(&repo_root).await)
}

#[tauri::command]
pub async fn buddy_git_checkout(
    service: State<'_, BuddyCoreService>,
    repo_root: String,
    branch: String,
) -> CmdResult<()> {
    service.git_checkout(&repo_root, &branch).await.map_err(err)
}

#[tauri::command]
pub async fn buddy_git_create_branch(
    service: State<'_, BuddyCoreService>,
    repo_root: String,
    branch: String,
) -> CmdResult<()> {
    service
        .git_create_branch(&repo_root, &branch)
        .await
        .map_err(err)
}

/// TS v1.2.11+: `buddy:generateCommitMessage` takes a single input object
/// and resolves to `{ message }`; any failure (cancel, timeout, non-zero
/// exit, invalid output) rejects.
#[derive(Debug, Clone, Serialize)]
pub struct GenerateCommitMessageResponse {
    pub message: String,
}

#[tauri::command]
pub async fn buddy_generate_commit_message(
    service: State<'_, BuddyCoreService>,
    input: GenerateCommitMessageInput,
) -> CmdResult<GenerateCommitMessageResponse> {
    service
        .generate_commit_message(input)
        .await
        .map(|message| GenerateCommitMessageResponse { message })
        .map_err(err)
}

#[tauri::command]
pub fn buddy_cancel_generate_commit_message(service: State<'_, BuddyCoreService>) {
    service.cancel_generate_commit_message();
}

#[tauri::command]
pub async fn buddy_test_launcher(
    service: State<'_, BuddyCoreService>,
    actor: String,
    command: String,
    env: Option<HashMap<String, String>>,
) -> CmdResult<TestLauncherResult> {
    service
        .test_launcher(&actor, &command, env)
        .await
        .map_err(err)
}

#[tauri::command]
pub async fn buddy_detect_actor_models(
    service: State<'_, BuddyCoreService>,
) -> CmdResult<HashMap<String, Option<String>>> {
    service.detect_actor_models().await.map_err(err)
}

#[tauri::command]
pub async fn buddy_update_task_text(
    service: State<'_, BuddyCoreService>,
    task_id: String,
    workspace_key: String,
    task_text: String,
) -> CmdResult<()> {
    service
        .update_task_text(&task_id, &workspace_key, &task_text)
        .await
        .map_err(err)
}

// ---------------------------------------------------------------------------
// Extra handlers (port of src/main/index.ts)
// ---------------------------------------------------------------------------

/// One clipboard file entry (`{ path, size }`), mirroring the Electron
/// handler's return shape.
#[derive(Debug, Clone, Serialize)]
pub struct ClipboardFilePath {
    pub path: String,
    pub size: u64,
}

/// TS: `clipboard:readFilePaths`. The Electron original reads
/// NSFilenamesPboardType / public.file-url off the macOS pasteboard; here we
/// shell out to `osascript` (no new dependency) asking for the clipboard as
/// file URLs and stat each POSIX path. Returns an empty list on any failure
/// and off macOS — same contract as the original.
#[tauri::command]
pub async fn read_clipboard_file_paths() -> CmdResult<Vec<ClipboardFilePath>> {
    if !cfg!(target_os = "macos") {
        return Ok(Vec::new());
    }
    let script = "set output to \"\"\n\
                  try\n\
                  \tset fileList to the clipboard as list of \u{ab}class furl\u{bb}\n\
                  on error\n\
                  \ttry\n\
                  \t\tset fileList to {(the clipboard as \u{ab}class furl\u{bb})}\n\
                  \ton error\n\
                  \t\treturn \"\"\n\
                  \tend try\n\
                  end try\n\
                  repeat with f in fileList\n\
                  \tset output to output & POSIX path of f & linefeed\n\
                  end repeat\n\
                  return output";
    let output = tokio::process::Command::new("osascript")
        .arg("-e")
        .arg(script)
        .output()
        .await;
    let Ok(output) = output else {
        return Ok(Vec::new());
    };
    if !output.status.success() {
        return Ok(Vec::new());
    }
    let paths = parse_clipboard_paths(&String::from_utf8_lossy(&output.stdout));
    let mut results = Vec::with_capacity(paths.len());
    for path in paths {
        let size = tokio::fs::metadata(&path)
            .await
            .map(|m| m.len())
            .unwrap_or(0);
        results.push(ClipboardFilePath { path, size });
    }
    Ok(results)
}

/// Extract POSIX paths from the osascript output: one per line, absolute
/// paths only (matches the original's `p.startsWith('/')` filter).
fn parse_clipboard_paths(text: &str) -> Vec<String> {
    text.lines()
        .map(|line| line.trim())
        .filter(|line| line.starts_with('/'))
        .map(String::from)
        .collect()
}

/// TS: `attachment:saveBuffer`. Writes the decoded buffer into the task's
/// `attachments/` directory under a random UUID filename (original extension
/// preserved) and returns the absolute file path.
#[tauri::command]
pub async fn save_attachment_buffer(
    service: State<'_, BuddyCoreService>,
    task_id: String,
    workspace_key: String,
    name: String,
    buffer_base64: String,
) -> CmdResult<String> {
    let bytes = BASE64.decode(&buffer_base64).map_err(err)?;
    let attachments_dir = service
        .get_store()
        .task_directory(&task_id, &workspace_key)
        .join("attachments");
    tokio::fs::create_dir_all(&attachments_dir)
        .await
        .map_err(err)?;
    let ext = attachment_extension(&name);
    let file_path = attachments_dir.join(format!("{}{}", uuid::Uuid::new_v4(), ext));
    tokio::fs::write(&file_path, &bytes).await.map_err(err)?;
    Ok(file_path.to_string_lossy().to_string())
}

/// TS: `name.includes('.') ? '.' + name.split('.').pop() : ''`.
fn attachment_extension(name: &str) -> String {
    if name.contains('.') {
        format!(".{}", name.rsplit('.').next().unwrap_or_default())
    } else {
        String::new()
    }
}

/// TS: `attachment:readFileAsDataURL`.
#[tauri::command]
pub async fn read_file_as_data_url(file_path: String, mime_type: String) -> CmdResult<String> {
    let bytes = tokio::fs::read(&file_path).await.map_err(err)?;
    Ok(format!("data:{mime_type};base64,{}", BASE64.encode(bytes)))
}

/// TS: `menu:updateLanguage` (ipcMain.on — fire-and-forget).
#[tauri::command]
pub fn update_menu_language(app: AppHandle, lang: String) {
    menu::update_menu_language(&app, &lang);
}

/// TS: `updater:check`. Since v1.2.14 failures reject the command (the
/// frontend bridge catches the rejection); progress still reaches the
/// frontend via `updater:event` emits.
#[tauri::command]
pub async fn updater_check(app: AppHandle) -> CmdResult<()> {
    updater::check_for_updates(&app).await
}

/// TS: `updater:download`.
#[tauri::command]
pub async fn updater_download(app: AppHandle) -> CmdResult<()> {
    updater::download_update(&app).await
}

/// TS: `updater:install` (quitAndInstall).
#[tauri::command]
pub async fn updater_install(app: AppHandle) -> CmdResult<()> {
    updater::install_update(&app).await
}

/// Dismissing the update-error notification: stop the periodic re-check
/// loop so the failed update is not retried automatically.
#[tauri::command]
pub fn updater_dismiss_error(app: AppHandle) {
    updater::stop_auto_retry(&app);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_clipboard_paths_keeps_absolute_paths() {
        let output = "/Users/a/file one.txt\n/Users/a/dir/\nsome noise\n\n  /Users/a/b.png  \n";
        assert_eq!(
            parse_clipboard_paths(output),
            vec![
                "/Users/a/file one.txt".to_string(),
                "/Users/a/dir/".to_string(),
                "/Users/a/b.png".to_string(),
            ]
        );
    }

    #[test]
    fn parse_clipboard_paths_empty_on_garbage() {
        assert!(parse_clipboard_paths("").is_empty());
        assert!(parse_clipboard_paths("not a path\nrelative/file\n").is_empty());
    }

    #[test]
    fn attachment_extension_matches_ts_split_pop() {
        assert_eq!(attachment_extension("photo.PNG"), ".PNG");
        assert_eq!(attachment_extension("archive.tar.gz"), ".gz");
        assert_eq!(attachment_extension("noext"), "");
        // TS: '.hidden'.split('.').pop() === 'hidden'
        assert_eq!(attachment_extension(".hidden"), ".hidden");
        // TS: 'a.'.split('.').pop() === ''
        assert_eq!(attachment_extension("a."), ".");
    }

    #[test]
    fn clipboard_file_path_serializes_to_ts_shape() {
        let value = serde_json::to_value(ClipboardFilePath {
            path: "/tmp/a.txt".to_string(),
            size: 12,
        })
        .unwrap();
        assert_eq!(value, serde_json::json!({ "path": "/tmp/a.txt", "size": 12 }));
    }
}
