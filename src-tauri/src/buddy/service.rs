//! `BuddyCoreService` — port of `src/main/buddy/service.ts` from the Electron
//! edition. Composes `BuddyStore` + `BuddyRunner` + `BuddyEventBus` +
//! `QueueCoordinator` and exposes the service surface the IPC commands
//! delegate to.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use serde::Serialize;

use super::commit_message::{self, GenerateCommitMessageInput};
use super::defaults::{normalize_global_settings, DEFAULT_LAUNCHER_ORDER};
use super::events::BuddyEventBus;
use super::git;
use super::launchers::{
    build_launcher_command, kind_needs_pty, parser_actor_for_kind, run_launcher,
    run_launcher_with_pty, split_command, LauncherCommandInput, PtyRunInput, RunLauncherInput,
};
use super::model_detect::detect_model_from_config;
use super::parsers::{parse_actor_events, parse_buddy_message, BuddyMessage};
use super::prompts::build_ping_prompt;
use super::queue_coordinator::{CoordinatorError, QueueCoordinator};
use super::runner::{
    collect_output_text, collect_raw_events, is_cli_warning_only, last_value, BuddyRunner,
    RunnerError, RunnerOptions, TaskNotifier,
};
use super::store::{BuddyStore, StoreError};
use super::types::{
    AttachmentMeta, BootstrapResponse, CountdownInput, CreateTaskInput, CreateTaskResult, Event,
    ExecutionMode, GitCommitPushResult, GitStatusResult, GlobalSettings, InstructionQueueItem,
    RoundEventSummary, SendMessageInput, StartTaskInput, Task, TaskDetail, TaskStats,
    TestLauncherResult,
};

#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    #[error("store error: {0}")]
    Store(#[from] StoreError),
    #[error("runner error: {0}")]
    Runner(#[from] RunnerError),
    #[error("coordinator error: {0}")]
    Coordinator(#[from] CoordinatorError),
    #[error("git error: {0}")]
    Git(#[from] git::GitError),
    #[error("{0}")]
    CommitMessage(#[from] commit_message::CommitMessageError),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Message(String),
}

impl ServiceError {
    fn msg(message: impl Into<String>) -> Self {
        ServiceError::Message(message.into())
    }
}

/// Response shape of `getEvents` (TS returns `{ events: Event[] }`).
#[derive(Debug, Clone, Serialize)]
pub struct EventsResponse {
    pub events: Vec<Event>,
}

/// Builds the production notifier. Used so the Tauri layer can inject the
/// `AppHandle`-dependent system notifier after the store exists.
pub type NotifierFactory = Box<dyn FnOnce(Arc<BuddyStore>) -> Arc<dyn TaskNotifier> + Send>;

/// Construction options, port of `BuddyCoreServiceOptions`. The TS edition
/// also accepts a bare string `dataRoot`; here use [`BuddyCoreServiceOptions::with_data_root`].
#[derive(Default)]
pub struct BuddyCoreServiceOptions {
    pub data_root: Option<PathBuf>,
    pub events: Option<BuddyEventBus>,
    /// Ready-made notifier. Wins over `notifier_factory` when both are set.
    pub notifier: Option<Arc<dyn TaskNotifier>>,
    pub notifier_factory: Option<NotifierFactory>,
    /// `app.getLocale()` equivalent. `None` detects the macOS system locale.
    pub locale: Option<String>,
}

impl BuddyCoreServiceOptions {
    pub fn with_data_root(data_root: impl Into<PathBuf>) -> Self {
        BuddyCoreServiceOptions {
            data_root: Some(data_root.into()),
            ..Default::default()
        }
    }
}

/// The core service. `Send + Sync`; managed as Tauri state by `commands.rs`.
pub struct BuddyCoreService {
    store: Arc<BuddyStore>,
    runner: Arc<BuddyRunner>,
    coordinator: QueueCoordinator,
    locale: Option<String>,
}

impl BuddyCoreService {
    pub fn new(options: BuddyCoreServiceOptions) -> Self {
        let store = Arc::new(BuddyStore::new(
            options.data_root.clone().unwrap_or_else(default_data_root),
        ));
        let notifier = match (options.notifier, options.notifier_factory) {
            (Some(notifier), _) => Some(notifier),
            (None, Some(factory)) => Some(factory(store.clone())),
            (None, None) => None,
        };
        let runner = Arc::new(BuddyRunner::new(
            store.clone(),
            RunnerOptions {
                execute_launchers: None,
                events: options.events.clone(),
                notifier,
            },
        ));
        let mut coordinator = QueueCoordinator::new(store.clone(), runner.clone());
        if let Some(events) = &options.events {
            coordinator = coordinator.with_events(events.clone());
        }
        let terminal_coordinator = coordinator.clone();
        runner.set_on_task_terminal(move |workspace_key: &str| {
            // TS: `void this.coordinator?.onTaskTerminal(workspaceKey)`.
            let coordinator = terminal_coordinator.clone();
            let workspace_key = workspace_key.to_string();
            tauri::async_runtime::spawn(async move {
                let _ = coordinator.on_task_terminal(&workspace_key).await;
            });
        });
        BuddyCoreService {
            store,
            runner,
            coordinator,
            locale: options.locale.or_else(detect_system_locale),
        }
    }

    pub fn get_coordinator(&self) -> &QueueCoordinator {
        &self.coordinator
    }

    pub fn get_store(&self) -> Arc<BuddyStore> {
        self.store.clone()
    }

    pub async fn update_task_text(
        &self,
        task_id: &str,
        workspace_key: &str,
        task_text: &str,
    ) -> Result<(), ServiceError> {
        Ok(self
            .store
            .update_task_text(task_id, workspace_key, task_text)
            .await?)
    }

    /// TS: `checkHealth` — the native service is always healthy.
    pub async fn check_health(&self) -> bool {
        true
    }

    pub async fn bootstrap(&self) -> Result<BootstrapResponse, ServiceError> {
        Ok(BootstrapResponse {
            version: Some("native".to_string()),
            repo_root: String::new(),
            data_root: self.store.data_root.to_string_lossy().to_string(),
            home_dir: dirs::home_dir()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default(),
            locale: self.locale.clone(),
            workspace_key: None,
            tasks: self.store.get_tasks().await,
            global_settings: Some(self.store.read_global_settings().await?),
        })
    }

    pub async fn get_tasks(&self) -> Vec<Task> {
        self.store.get_tasks().await
    }

    pub async fn get_task_detail(
        &self,
        task_id: &str,
        workspace_key: Option<&str>,
    ) -> Result<TaskDetail, ServiceError> {
        let workspace_key = require_workspace_key(workspace_key)?;
        Ok(self.store.get_task_detail(task_id, workspace_key).await?)
    }

    pub async fn create_task(&self, input: CreateTaskInput) -> Result<CreateTaskResult, ServiceError> {
        let result = self.store.create_task(input).await?;
        // A newly created queued task may be the next to run; an immediate task
        // may now block the queue. Either way the workspace scheduling
        // conditions changed, so re-evaluate once.
        spawn_reconcile(&self.coordinator, &result.workspace_key);
        Ok(result)
    }

    pub async fn delete_task(
        &self,
        task_id: &str,
        workspace_key: Option<&str>,
    ) -> Result<(), ServiceError> {
        let workspace_key = require_workspace_key(workspace_key)?;
        self.store.delete_task(task_id, workspace_key).await?;
        // Removing a blocking task may unblock the queue.
        spawn_on_task_terminal(&self.coordinator, workspace_key);
        Ok(())
    }

    pub async fn start_task(
        &self,
        task_id: &str,
        input: StartTaskInput,
    ) -> Result<(), ServiceError> {
        let workspace_key = input
            .workspace_key
            .clone()
            .filter(|k| !k.is_empty())
            .ok_or_else(|| ServiceError::msg("workspace_key is required"))?;
        let state = self.store.read_task_state(task_id, &workspace_key).await.ok();
        // A queued task must not be started directly by the renderer via
        // runner.startTask, because runner.canStartFrom('QUEUED') is false. Any
        // manual user start on a queued task goes through the coordinator's
        // manual activation path, which preserves queue identity and supersede
        // logic.
        if state
            .as_ref()
            .and_then(|s| s.execution_mode)
            == Some(ExecutionMode::Queued)
        {
            self.coordinator.start_queued_now(task_id, &workspace_key).await?;
            return Ok(());
        }
        self.runner.start_task(task_id, input).await?;
        // After an immediate start, re-evaluate the queue in case this task is
        // itself an immediate-execution task that should block queued
        // advancement.
        spawn_reconcile(&self.coordinator, &workspace_key);
        Ok(())
    }

    pub async fn send_message(
        &self,
        task_id: &str,
        input: SendMessageInput,
    ) -> Result<(), ServiceError> {
        Ok(self.runner.send_message(task_id, input).await?)
    }

    pub async fn skip_countdown(
        &self,
        task_id: &str,
        input: CountdownInput,
    ) -> Result<(), ServiceError> {
        self.runner.skip_countdown(task_id, input).await?;
        Ok(())
    }

    pub async fn pause_countdown(
        &self,
        task_id: &str,
        input: CountdownInput,
    ) -> Result<(), ServiceError> {
        Ok(self.runner.pause_countdown(task_id, input).await?)
    }

    pub async fn interrupt(
        &self,
        task_id: &str,
        workspace_key: Option<&str>,
    ) -> Result<(), ServiceError> {
        let workspace_key = require_workspace_key(workspace_key)?;
        self.runner.interrupt(task_id, workspace_key).await?;
        // A user interrupt moves the task to PAUSED, which may block (queued)
        // or free (immediate) the workspace queue. Re-evaluate once.
        spawn_on_task_terminal(&self.coordinator, workspace_key);
        Ok(())
    }

    pub async fn enqueue_instruction(
        &self,
        task_id: &str,
        workspace_key: &str,
        content: &str,
        attachments: Option<Vec<AttachmentMeta>>,
    ) -> Result<InstructionQueueItem, ServiceError> {
        Ok(self
            .runner
            .enqueue_instruction(task_id, workspace_key, content, attachments)
            .await?)
    }

    pub async fn dequeue_instruction(
        &self,
        task_id: &str,
        workspace_key: &str,
        item_id: &str,
    ) -> Result<(), ServiceError> {
        Ok(self
            .runner
            .dequeue_instruction(task_id, workspace_key, item_id)
            .await?)
    }

    pub async fn clear_instruction_queue(
        &self,
        task_id: &str,
        workspace_key: &str,
    ) -> Result<(), ServiceError> {
        Ok(self
            .runner
            .clear_instruction_queue(task_id, workspace_key)
            .await?)
    }

    pub async fn interrupt_and_insert(
        &self,
        task_id: &str,
        workspace_key: &str,
        queue_item_id: &str,
    ) -> Result<(), ServiceError> {
        Ok(self
            .runner
            .interrupt_and_insert(task_id, workspace_key, queue_item_id)
            .await?)
    }

    pub async fn get_events(
        &self,
        task_id: &str,
        since: u64,
        workspace_key: Option<&str>,
    ) -> Result<EventsResponse, ServiceError> {
        let workspace_key = require_workspace_key(workspace_key)?;
        Ok(EventsResponse {
            events: self.store.get_events(task_id, since, workspace_key).await,
        })
    }

    pub async fn get_round_events(
        &self,
        task_id: &str,
        run_id: &str,
        workspace_key: Option<&str>,
        actor: Option<&str>,
        command: Option<&str>,
    ) -> Result<Option<RoundEventSummary>, ServiceError> {
        let workspace_key = require_workspace_key(workspace_key)?;
        Ok(self
            .store
            .get_round_events(task_id, run_id, workspace_key, actor, command)
            .await)
    }

    pub async fn get_task_stats(
        &self,
        task_id: &str,
        workspace_key: Option<&str>,
    ) -> Result<Option<TaskStats>, ServiceError> {
        let workspace_key = require_workspace_key(workspace_key)?;
        Ok(self.store.get_task_stats(task_id, workspace_key).await)
    }

    pub async fn update_global_settings(
        &self,
        settings: &GlobalSettings,
    ) -> Result<GlobalSettings, ServiceError> {
        Ok(self.store.update_global_settings(settings).await?)
    }

    pub async fn git_status(&self, repo_root: &str) -> GitStatusResult {
        git::get_git_status(repo_root).await
    }

    pub async fn git_stage_all(&self, repo_root: &str) -> Result<(), ServiceError> {
        Ok(git::git_stage_all(repo_root).await?)
    }

    pub async fn git_stage_files(
        &self,
        repo_root: &str,
        paths: &[String],
    ) -> Result<(), ServiceError> {
        Ok(git::git_stage_files(repo_root, paths).await?)
    }

    pub async fn git_commit_and_push(
        &self,
        repo_root: &str,
        message: &str,
        remote: &str,
        push: bool,
    ) -> Result<GitCommitPushResult, ServiceError> {
        Ok(git::git_commit_and_push(repo_root, message, remote, push).await?)
    }

    pub async fn git_diff_for_commit_message(
        &self,
        repo_root: &str,
        paths: Option<&[String]>,
    ) -> String {
        git::git_diff_for_commit_message(repo_root, paths).await
    }

    pub async fn git_file_diff(&self, repo_root: &str, file_path: &str) -> String {
        git::git_file_diff(repo_root, file_path).await
    }

    pub async fn git_branches(&self, repo_root: &str) -> Vec<String> {
        git::git_branches(repo_root).await
    }

    pub async fn git_checkout(&self, repo_root: &str, branch: &str) -> Result<(), ServiceError> {
        Ok(git::git_checkout(repo_root, branch).await?)
    }

    pub async fn git_create_branch(
        &self,
        repo_root: &str,
        branch: &str,
    ) -> Result<(), ServiceError> {
        Ok(git::git_create_branch(repo_root, branch).await?)
    }

    /// Port of the v1.2.11+ `generateCommitMessage(input)`: resolves the
    /// actor (unsupported → 'claude') and its launcher (task → global →
    /// default), then runs the one-shot generation. Failures — including
    /// cancellation — are errors; the renderer treats any rejection as
    /// generate-failed.
    pub async fn generate_commit_message(
        &self,
        input: GenerateCommitMessageInput,
    ) -> Result<String, ServiceError> {
        let actor = if commit_message::is_supported_actor(&input.actor) {
            input.actor.clone()
        } else {
            "claude".to_string()
        };
        let global_settings = self.store.read_global_settings().await?;
        let launcher = commit_message::resolve_launcher(
            &actor,
            input.task_settings.as_ref(),
            Some(&global_settings),
        );
        let result = commit_message::generate_commit_message_with_actor(
            &commit_message::GenerateCommitMessageActorInput {
                repo_root: input.repo_root,
                actor,
                lang: input.lang,
                paths: input.paths,
                launcher,
            },
        )
        .await?;
        Ok(result.message)
    }

    pub fn cancel_generate_commit_message(&self) {
        commit_message::cancel_generate_commit_message()
    }

    /// Startup recovery (TS `recoverInterruptedRuns`): pause every task left
    /// in RUNNING_*/PINGING (persisted + published by the runner), then
    /// rebuild per-workspace queues and run a safe scheduling pass. A
    /// previously-running queued task is now PAUSED and blocks its queue — no
    /// auto-start. Unblocked workspaces with waiting tasks start their queue
    /// head.
    pub async fn recover_interrupted_runs(&self) -> Result<(), ServiceError> {
        self.runner.recover_interrupted_runs().await?;
        self.coordinator.rebuild_and_reconcile_all().await?;
        Ok(())
    }

    /// Detect the currently configured model for every actor, reading each
    /// actor's launcher command from global settings so the result matches
    /// what the runner will actually invoke. Actors whose model cannot be
    /// determined before a run (e.g. claude, whose model is only emitted in
    /// stream-json output) resolve to `None`.
    pub async fn detect_actor_models(
        &self,
    ) -> Result<HashMap<String, Option<String>>, ServiceError> {
        let settings = self.store.read_global_settings().await?;
        let normalized = normalize_global_settings(Some(&settings));
        let launchers = normalized.launchers.unwrap_or_default();
        let mut result: HashMap<String, Option<String>> = HashMap::new();
        for actor in DEFAULT_LAUNCHER_ORDER {
            let command = launchers.get(actor).map(|l| l.command.as_str());
            result.insert(
                actor.to_string(),
                detect_model_from_config(actor, command).await,
            );
        }
        Ok(result)
    }

    /// Two-phase launcher check, port of `testLauncher`. Phase 1 (`tool_check`)
    /// verifies the command can be spawned; phase 2 (`ping`) invokes the actor
    /// with a hello prompt in a temp directory and validates the buddy-message
    /// response. Filesystem setup failures propagate (TS: they reject the
    /// handler); launcher failures are reported in the result.
    pub async fn test_launcher(
        &self,
        actor: &str,
        command: &str,
        env: Option<HashMap<String, String>>,
    ) -> Result<TestLauncherResult, ServiceError> {
        const PING_TIMEOUT_MS: u64 = 120_000;
        let env = env.unwrap_or_default();

        // Phase 1: Tool check — verify the command exists and can be spawned.
        let base_executable = split_command(command.trim())
            .into_iter()
            .next()
            .unwrap_or_default();
        let spawn_result = tokio::process::Command::new(&base_executable)
            .arg("--version")
            .envs(&env)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        match spawn_result {
            Ok(mut child) => {
                // Spawn succeeded; the probe process is no longer needed.
                let _ = child.kill().await;
            }
            Err(error) => {
                return Ok(TestLauncherResult {
                    actor: actor.to_string(),
                    success: false,
                    phase: "tool_check".to_string(),
                    error: Some(truncate(&error.to_string(), 300)),
                    session_id: None,
                    thread_id: None,
                    response_preview: None,
                });
            }
        }

        // Phase 2: Ping test — actually invoke the actor with a hello prompt.
        let millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or_default();
        let test_dir = std::env::temp_dir().join(format!("buddy-test-{actor}-{millis}"));
        let result = run_ping(actor, command, &env, &test_dir, millis, PING_TIMEOUT_MS).await;
        // Clean up temp directory (ignore cleanup errors, like the TS finally).
        let _ = tokio::fs::remove_dir_all(&test_dir).await;
        result
    }
}

/// Phase 2 of `testLauncher`, kept separate so the temp directory is always
/// cleaned up by the caller.
async fn run_ping(
    actor: &str,
    command: &str,
    env: &HashMap<String, String>,
    test_dir: &std::path::Path,
    millis: u128,
    ping_timeout_ms: u64,
) -> Result<TestLauncherResult, ServiceError> {
    tokio::fs::create_dir_all(test_dir).await?;
    let uuid = uuid::Uuid::new_v4().simple().to_string();
    let run_id = format!("test_{millis}_{}", &uuid[..6]);
    let prompt = build_ping_prompt(actor);
    let prompt_file = test_dir.join(format!("{run_id}-prompt.md"));
    let output_file = test_dir.join(format!("{run_id}-output.md"));
    let event_file = test_dir.join(format!("{run_id}-events.jsonl"));
    tokio::fs::write(&prompt_file, &prompt).await?;

    let test_dir_string = test_dir.to_string_lossy().to_string();
    let launcher_command = build_launcher_command(&LauncherCommandInput {
        actor: actor.to_string(),
        command: command.to_string(),
        mode: Some("start".to_string()),
        prompt_file: prompt_file.to_string_lossy().to_string(),
        prompt_text: Some(prompt),
        event_file: Some(event_file.to_string_lossy().to_string()),
        output_file: Some(output_file.to_string_lossy().to_string()),
        repo_root: Some(test_dir_string.clone()),
        task_dir: Some(test_dir_string.clone()),
        run_id: Some(run_id),
        session_id: None,
    });

    let mut output_lines: Vec<String> = Vec::new();
    let mut stderr_lines: Vec<String> = Vec::new();
    // TS: `{ ...env, ...(launcherCommand.env ?? {}) }`.
    let mut merged_env = env.clone();
    if let Some(launcher_env) = &launcher_command.env {
        merged_env.extend(launcher_env.clone());
    }

    let run_result = if kind_needs_pty(launcher_command.kind) {
        run_launcher_with_pty(
            &PtyRunInput {
                command: launcher_command.command.clone(),
                args: launcher_command.args.clone(),
                cwd: test_dir_string.clone(),
                env: Some(merged_env),
                timeout_ms: ping_timeout_ms,
                abort: None,
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
                cwd: test_dir_string.clone(),
                env: Some(merged_env),
                stdin_text: launcher_command.stdin_text.clone(),
                timeout_ms: ping_timeout_ms,
                abort: None,
            },
            |line| output_lines.push(line),
            |line| stderr_lines.push(line),
        )
        .await
    };

    let result = match run_result {
        Ok(result) => result,
        Err(error) => {
            let stderr_text = stderr_lines.join("\n").trim().to_string();
            let is_only_warning =
                !stderr_text.is_empty() && is_cli_warning_only(&stderr_text);
            let message = error.to_string();
            let error_text = if !message.is_empty() {
                message
            } else if !is_only_warning {
                stderr_text
            } else {
                "Actor exited without producing any output".to_string()
            };
            return Ok(TestLauncherResult {
                actor: actor.to_string(),
                success: false,
                phase: "ping".to_string(),
                error: Some(truncate(&error_text, 300)),
                session_id: None,
                thread_id: None,
                response_preview: None,
            });
        }
    };

    let stdout_text = output_lines.join("\n");
    let raw_events = collect_raw_events(&event_file, &stdout_text, launcher_command.kind).await;
    let output_text =
        collect_output_text(actor, launcher_command.kind, &output_file, &stdout_text).await;
    let parsed_lines = parse_actor_events(
        &parser_actor_for_kind(actor, launcher_command.kind),
        &raw_events,
    );

    if result.exit_code != Some(0) {
        let stderr_text = stderr_lines.join("\n").trim().to_string();
        let error = if !stderr_text.is_empty() {
            stderr_text
        } else if !output_text.trim().is_empty() {
            output_text.trim().to_string()
        } else {
            match result.exit_code {
                Some(code) => format!("Process exited with code {code}"),
                // TS stringifies a null exit code as "null".
                None => "Process exited with code null".to_string(),
            }
        };
        return Ok(TestLauncherResult {
            actor: actor.to_string(),
            success: false,
            phase: "ping".to_string(),
            error: Some(truncate(&error, 300)),
            session_id: None,
            thread_id: None,
            response_preview: None,
        });
    }

    // Verify the actor responded with a valid buddy message.
    let message = parse_buddy_message(&output_text);
    let has_content = match &message {
        BuddyMessage::Message { text } => !text.trim().is_empty(),
        BuddyMessage::Break { content, .. } => !content.trim().is_empty(),
    };
    if !has_content {
        return Ok(TestLauncherResult {
            actor: actor.to_string(),
            success: false,
            phase: "ping".to_string(),
            error: Some("Actor responded with empty content".to_string()),
            session_id: None,
            thread_id: None,
            response_preview: None,
        });
    }

    let session_id = last_value(parsed_lines.iter().map(|line| line.session_id.clone()));
    let thread_id = last_value(parsed_lines.iter().map(|line| line.thread_id.clone()));
    let preview = match &message {
        BuddyMessage::Message { text } => truncate(text, 200),
        BuddyMessage::Break { content, .. } => truncate(content, 200),
    };

    Ok(TestLauncherResult {
        actor: actor.to_string(),
        success: true,
        phase: "ping".to_string(),
        error: None,
        session_id,
        thread_id,
        response_preview: Some(preview),
    })
}

fn require_workspace_key(workspace_key: Option<&str>) -> Result<&str, ServiceError> {
    workspace_key
        .filter(|k| !k.is_empty())
        .ok_or_else(|| ServiceError::msg("workspaceKey is required"))
}

/// JS-style `slice(0, n)` by characters.
fn truncate(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

/// Fire-and-forget queue re-evaluation (TS: `void this.coordinator?.reconcile(...)`).
fn spawn_reconcile(coordinator: &QueueCoordinator, workspace_key: &str) {
    let coordinator = coordinator.clone();
    let workspace_key = workspace_key.to_string();
    tauri::async_runtime::spawn(async move {
        let _ = coordinator.reconcile(&workspace_key).await;
    });
}

/// Fire-and-forget terminal notification (TS: `void this.coordinator?.onTaskTerminal(...)`).
fn spawn_on_task_terminal(coordinator: &QueueCoordinator, workspace_key: &str) {
    let coordinator = coordinator.clone();
    let workspace_key = workspace_key.to_string();
    tauri::async_runtime::spawn(async move {
        let _ = coordinator.on_task_terminal(&workspace_key).await;
    });
}

/// TS: `join(homedir(), 'Library', 'Application Support', 'buddy')`.
pub fn default_data_root() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join("Library")
        .join("Application Support")
        .join("buddy")
}

/// `app.getLocale()` equivalent without new dependencies: ask macOS for the
/// user's preferred locale (`en_US`-style). Returns `None` off macOS or when
/// the lookup fails.
pub fn detect_system_locale() -> Option<String> {
    let output = std::process::Command::new("defaults")
        .args(["read", "-g", "AppleLocale"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let locale = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if locale.is_empty() {
        None
    } else {
        Some(locale)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buddy::types::Launcher;

    fn service_in(dir: &std::path::Path) -> BuddyCoreService {
        BuddyCoreService::new(BuddyCoreServiceOptions {
            data_root: Some(dir.to_path_buf()),
            locale: Some("en-US".to_string()),
            ..Default::default()
        })
    }

    // -----------------------------------------------------------------------
    // Port of tests/unit/main/buddy-service.test.ts
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn check_health_reports_native_health_without_http() {
        let dir = tempfile::tempdir().unwrap();
        let service = service_in(dir.path());
        assert!(service.check_health().await);
    }

    #[tokio::test]
    async fn default_data_root_is_application_support_buddy() {
        // TS: bootstrap().data_root === join(homedir(), 'Library', 'Application Support', 'buddy')
        let expected = dirs::home_dir()
            .unwrap()
            .join("Library")
            .join("Application Support")
            .join("buddy");
        assert_eq!(default_data_root(), expected);
        // The service constructed without an explicit root uses it (without
        // touching the real directory).
        let service = BuddyCoreService::new(BuddyCoreServiceOptions {
            locale: Some("en-US".to_string()),
            ..Default::default()
        });
        assert_eq!(service.get_store().data_root, expected);
    }

    #[tokio::test]
    async fn bootstrap_returns_native_global_cli_settings() {
        let dir = tempfile::tempdir().unwrap();
        let service = service_in(dir.path());
        let bootstrap = service.bootstrap().await.unwrap();

        assert_eq!(bootstrap.version.as_deref(), Some("native"));
        assert_eq!(bootstrap.data_root, dir.path().to_string_lossy());
        assert_eq!(bootstrap.locale.as_deref(), Some("en-US"));
        assert!(bootstrap.tasks.is_empty());
        let settings = bootstrap.global_settings.unwrap();
        let launchers = settings.launchers.unwrap();
        assert_eq!(launchers["claude"].command, "claude");
        assert_eq!(launchers["codex"].command, "codex");
    }

    // -----------------------------------------------------------------------
    // Port of tests/unit/main/buddy-settings.test.ts (service-level view)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn default_global_settings_when_no_file_exists() {
        let dir = tempfile::tempdir().unwrap();
        let service = service_in(dir.path());
        let settings = service.get_store().read_global_settings().await.unwrap();

        assert_eq!(settings.protocol_version.as_deref(), Some("1"));
        assert_eq!(settings.countdown_seconds, Some(30));
        assert_eq!(settings.max_rounds, Some(9999));
        assert_eq!(settings.max_consecutive_failures, Some(10));
        let launchers = settings.launchers.unwrap();
        let empty_env: HashMap<String, String> = HashMap::new();
        let expected = [
            ("claude", "claude"),
            ("codex", "codex"),
            ("cursor", "cursor-agent"),
        ];
        for (actor, command) in expected {
            let launcher = launchers.get(actor).unwrap();
            assert_eq!(launcher.command, command);
            assert_eq!(launcher.env, empty_env);
            assert_eq!(launcher.timeout_seconds, 7200);
        }
        assert_eq!(settings.seed_claude_session_id.as_deref(), Some(""));
        assert_eq!(settings.seed_codex_thread_id.as_deref(), Some(""));
    }

    #[tokio::test]
    async fn update_global_settings_persists_and_normalizes() {
        let dir = tempfile::tempdir().unwrap();
        let service = service_in(dir.path());

        service
            .update_global_settings(&GlobalSettings {
                countdown_seconds: Some(45),
                ..Default::default()
            })
            .await
            .unwrap();

        let raw = tokio::fs::read_to_string(dir.path().join("global").join("settings.json"))
            .await
            .unwrap();
        assert!(
            raw.contains("\"countdown_seconds\": 45"),
            "settings.json should contain the new countdown: {raw}"
        );
        let settings = service.get_store().read_global_settings().await.unwrap();
        assert_eq!(settings.countdown_seconds, Some(45));
        assert_eq!(
            settings.launchers.unwrap().get("claude").unwrap().command,
            "claude"
        );
    }

    #[tokio::test]
    async fn reads_legacy_root_global_settings() {
        let dir = tempfile::tempdir().unwrap();
        let service = service_in(dir.path());
        let legacy = serde_json::json!({
            "countdown_seconds": 10,
            "launchers": {
                "claude": {
                    "command": "wecode --dangerously-skip-permissions",
                    "env": {},
                    "timeout_seconds": 7200
                }
            }
        });
        tokio::fs::write(
            dir.path().join("global_settings.json"),
            serde_json::to_string(&legacy).unwrap(),
        )
        .await
        .unwrap();

        let settings = service.get_store().read_global_settings().await.unwrap();
        assert_eq!(settings.countdown_seconds, Some(10));
        assert_eq!(
            settings.launchers.unwrap().get("claude").unwrap().command,
            "wecode --dangerously-skip-permissions"
        );
    }

    #[tokio::test]
    async fn create_and_delete_task_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let service = service_in(dir.path());
        let created = service
            .create_task(CreateTaskInput {
                task_id: "demo".to_string(),
                repo_root: Some("/tmp/repo".to_string()),
                task_text: None,
                context_text: None,
                settings: None,
                execution_mode: None,
            })
            .await
            .unwrap();

        assert!(std::path::Path::new(&created.path).exists());
        service
            .delete_task("demo", Some(&created.workspace_key))
            .await
            .unwrap();
        assert!(!std::path::Path::new(&created.path).exists());
    }

    #[tokio::test]
    async fn task_crud_delegation_requires_workspace_key() {
        let dir = tempfile::tempdir().unwrap();
        let service = service_in(dir.path());
        let err = service.get_task_detail("demo", None).await.unwrap_err();
        assert!(err.to_string().contains("workspaceKey"));
        let err = service.delete_task("demo", None).await.unwrap_err();
        assert!(err.to_string().contains("workspaceKey"));
        let err = service.get_events("demo", 0, None).await.unwrap_err();
        assert!(err.to_string().contains("workspaceKey"));
    }

    #[tokio::test]
    async fn test_launcher_reports_tool_check_failure_for_missing_command() {
        let dir = tempfile::tempdir().unwrap();
        let service = service_in(dir.path());
        let result = service
            .test_launcher("claude", "buddy-definitely-not-a-real-command-xyz", None)
            .await
            .unwrap();
        assert!(!result.success);
        assert_eq!(result.phase, "tool_check");
        assert!(result.error.unwrap().len() <= 300);
    }

    #[tokio::test]
    async fn detect_actor_models_covers_all_default_actors() {
        let dir = tempfile::tempdir().unwrap();
        let service = service_in(dir.path());
        let models = service.detect_actor_models().await.unwrap();
        for actor in DEFAULT_LAUNCHER_ORDER {
            assert!(models.contains_key(actor), "missing actor {actor}");
        }
        assert_eq!(models.len(), DEFAULT_LAUNCHER_ORDER.len());
    }

    #[tokio::test]
    async fn detect_actor_models_uses_launcher_command_from_settings() {
        let dir = tempfile::tempdir().unwrap();
        let service = service_in(dir.path());
        let mut launchers = HashMap::new();
        launchers.insert(
            "codex".to_string(),
            Launcher {
                command: "codex -m gpt-5.6-luna".to_string(),
                env: HashMap::new(),
                timeout_seconds: 7200,
            },
        );
        service
            .update_global_settings(&GlobalSettings {
                launchers: Some(launchers),
                ..Default::default()
            })
            .await
            .unwrap();
        // Just verify the flow reads settings and still resolves every actor.
        let models = service.detect_actor_models().await.unwrap();
        assert_eq!(models.len(), DEFAULT_LAUNCHER_ORDER.len());
    }
}
