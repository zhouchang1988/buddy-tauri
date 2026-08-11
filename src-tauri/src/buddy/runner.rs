//! Task runner state machine + actor subprocess orchestration.
//! Port of `src/main/buddy/runner.ts` from the Electron edition.
//!
//! State machine: READY → RUNNING_{ACTOR} → (READY | PAUSED | DONE) / FAILED,
//! with dual-break termination (both actors must signal `type=break`),
//! consecutive-failure limiting, compact (context-window) session resets,
//! upgrade auto-retries, connectivity health checks (PINGING), countdown
//! pause/skip, and instruction-queue draining between rounds.
//!
//! Interrupt semantics mirror the Electron edition: `interrupt` /
//! `interrupt_and_insert` clear `active_run` and move the task to PAUSED; the
//! in-flight launcher process is NOT killed (the TS original never killed it
//! either — `completeActor`/`markFailed` guard on `active_run.run_id` and
//! no-op when it changed). A hard child-kill would need a cancellation handle
//! out of `launchers.rs`, which is out of this module's scope.

use crate::buddy::events::BuddyEventBus;
use crate::buddy::launchers::{
    build_launcher_command, command_kind_for, kind_needs_pty, parser_actor_for_kind, run_launcher,
    run_launcher_with_pty, LauncherCommand, LauncherCommandKind, LauncherCommandInput,
    LauncherError, LauncherRunResult, PtyRunInput, RunLauncherInput,
};
use crate::buddy::locks::{create_run_lock, remove_run_lock};
use crate::buddy::parsers::{
    extract_actor_output, parse_actor_events, parse_actor_line, parse_buddy_message,
    parse_jsonl_buffer, BuddyMessage, ParsedActorLine,
};
use crate::buddy::prompts::{
    actor_display_name, build_actor_prompt, build_ping_prompt, hash_text,
    implementer_actor as resolve_implementer_actor, next_actor as next_actor_for_settings,
    BuildActorPromptInput, TranscriptRow,
};
use crate::buddy::queue_coordinator::QueueTaskRunner;
use crate::buddy::store::{BuddyStore, EventInput, StoreError};
use crate::buddy::types::{
    ActiveRun, AttachmentMeta, BreakMarker, Countdown, CountdownInput, Event, Failure,
    GlobalSettings, HealthCheckResult, InstructionQueueItem, Launcher, SendMessageInput,
    StartTaskInput, TaskDetail, TaskEventEnvelope, TaskSettings, TaskState, TaskStatus,
    TranscriptEntry,
};
use async_trait::async_trait;
use chrono::Utc;
use parking_lot::Mutex;
use regex::Regex;
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::OnceLock;

const PING_TIMEOUT_SECONDS: u64 = 120;
const DEFAULT_MAX_COMPACT_RETRIES: u32 = 3;
const DEFAULT_MAX_UPGRADE_RETRIES: u32 = 3;
const UPGRADE_WAIT_MS: u64 = 5000;

/// Shim script placed at `<data_root>/shims/osascript` and prepended to PATH
/// for PTY (opencode) child processes. opencode plugins (e.g. oh-my-opencode's
/// session-notification hook) call `osascript -e 'display notification ...'`
/// when a run completes and AWAIT it; under a GUI parent that lacks macOS
/// automation consent this call blocks on a consent dialog, so the opencode
/// process never exits and the round never advances. The shim swallows
/// `display notification` calls (Buddy has its own notification system) and
/// passes everything else through to the real osascript.
const OSASCRIPT_SHIM: &str = r#"#!/bin/sh
# Buddy shim: no-op `display notification` calls from actor-CLI plugins.
# See runner.rs OSASCRIPT_SHIM.
case "$*" in
  *"display notification"*) exit 0 ;;
esac
exec /usr/bin/osascript "$@"
"#;

/// Ensure the [`OSASCRIPT_SHIM`] exists under `<data_root>/shims/osascript`
/// and return the directory to prepend to a child process PATH. Best-effort:
/// returns `None` if the shim cannot be written (the run then proceeds
/// without it, matching the pre-shim behavior).
async fn ensure_osascript_shim_dir(data_root: &Path) -> Option<std::path::PathBuf> {
    let dir = data_root.join("shims");
    let shim = dir.join("osascript");
    let needs_write = match tokio::fs::read_to_string(&shim).await {
        Ok(existing) => existing != OSASCRIPT_SHIM,
        Err(_) => true,
    };
    if needs_write {
        tokio::fs::create_dir_all(&dir).await.ok()?;
        tokio::fs::write(&shim, OSASCRIPT_SHIM).await.ok()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = tokio::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).await;
        }
    }
    Some(dir)
}

/// Prepend `dir` to `PATH` inside an env map (falls back to the current
/// process PATH when the map has no explicit PATH entry).
fn prepend_path(env: &mut HashMap<String, String>, dir: &Path) {
    let current = env
        .get("PATH")
        .cloned()
        .unwrap_or_else(|| std::env::var("PATH").unwrap_or_default());
    env.insert(
        "PATH".to_string(),
        format!("{}:{}", dir.to_string_lossy(), current),
    );
}


/// Canonical phrase used in both throw sites and the context-limit patterns.
const CONTEXT_EXHAUSTED_PHRASE: &str = "context window exhausted";

const SUMMARIZE_MAX_TRANSCRIPT_ENTRIES: usize = 10;
const SUMMARIZE_MAX_ENTRY_CHARS: usize = 2000;
const SUMMARIZE_MAX_PROMPT_CHARS: usize = 50000;

fn context_window_limit_patterns() -> &'static [Regex] {
    static PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        [
            r"context window limit",
            r"context length exceeded",
            r"context\.length\.exceeded",
            r"maximum context length",
            r"max.*context.*length",
            r"token limit",
            r"too many tokens",
            r"exceeds.*token",
            r"exceeded.*token",
            r"input.*too long",
            r"request too large",
            r"context window.*exhausted",
            // Chinese error messages from models like GLM, Qwen, DeepSeek
            r"对话内容太长",
            r"超出.*处理能力",
            r"超出.*上下文",
            r"超出.*模型.*能力",
            r"上下文.*超限",
            r"上下文.*超出",
            r"内容过长",
            r"超出.*长度",
        ]
        .iter()
        .map(|p| Regex::new(&format!("(?i){p}")).unwrap())
        .collect()
    })
}

fn upgrade_patterns() -> &'static [Regex] {
    static PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        [
            r"upgrade.*complete",
            r"updated.*restart",
            r"restart.*required",
            r"new version",
            r"auto.?update",
            r"自动更新",
            r"自动升级",
            r"升级完成",
            r"请重启",
            r"已更新",
        ]
        .iter()
        .map(|p| Regex::new(&format!("(?i){p}")).unwrap())
        .collect()
    })
}

fn cli_warning_patterns() -> &'static [Regex] {
    static PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        [
            r"Running with --dangerously-skip-permissions",
            r"Warning:.*skip.*permission",
            r"bypass.*approval",
            r"bypass.*sandbox",
        ]
        .iter()
        .map(|p| Regex::new(&format!("(?i){p}")).unwrap())
        .collect()
    })
}

/// Check if an error/stderr message indicates the child exited for an auto-upgrade.
pub fn is_upgrade_exit_error(message: &str) -> bool {
    upgrade_patterns().iter().any(|p| p.is_match(message))
}

/// Check if an error message indicates a context window limit error.
pub fn is_context_window_limit_error(message: &str) -> bool {
    context_window_limit_patterns()
        .iter()
        .any(|p| p.is_match(message))
}

/// Check if stderr text contains only known CLI warnings (not real errors).
pub fn is_cli_warning_only(stderr_text: &str) -> bool {
    let lines: Vec<&str> = stderr_text
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .filter(|l| !l.trim().is_empty())
        .collect();
    !lines.is_empty()
        && lines
            .iter()
            .all(|line| cli_warning_patterns().iter().any(|p| p.is_match(line)))
}

/// Human-readable exit summary, port of `exitErrorMessage`.
pub fn exit_error_message(exit_code: Option<i32>, signal: Option<&str>) -> String {
    match exit_code {
        None => match signal {
            Some(signal) => format!("Actor was killed by signal {signal} (possible timeout)"),
            None => "Actor exited unexpectedly (no exit code)".to_string(),
        },
        Some(code) => format!("Actor exited with code {code}"),
    }
}

/// Last `Some` value of an iterator, port of `lastValue`.
pub fn last_value<I>(values: I) -> Option<String>
where
    I: IntoIterator<Item = Option<String>>,
{
    values.into_iter().flatten().last()
}

#[derive(Debug, thiserror::Error)]
pub enum RunnerError {
    #[error("store error: {0}")]
    Store(#[from] StoreError),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("launcher error: {0}")]
    Launcher(#[from] LauncherError),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Message(String),
}

impl RunnerError {
    fn msg(message: impl Into<String>) -> Self {
        RunnerError::Message(message.into())
    }
}

/// System-notification sink, port of the `TaskNotifier` interface in
/// `src/main/buddy/notifications.ts`. The concrete implementation lands with
/// `notifications.rs`; the runner only depends on this trait.
#[async_trait]
pub trait TaskNotifier: Send + Sync {
    async fn notify_task_done(
        &self,
        task_id: &str,
        workspace_key: &str,
        reason: &str,
        first_actor: Option<&str>,
        second_actor: Option<&str>,
    );
    async fn notify_task_failed(
        &self,
        task_id: &str,
        workspace_key: &str,
        actor: &str,
        error: &str,
    );
    async fn notify_task_paused(
        &self,
        task_id: &str,
        workspace_key: &str,
        actor: &str,
        consecutive_failures: u32,
        max_failures: u32,
    );
}

/// Construction options, port of `RunnerOptions`.
#[derive(Default)]
pub struct RunnerOptions {
    /// `false` skips launcher execution (and the health check) — the state
    /// machine transitions only. Defaults to `true`, like the TS original.
    pub execute_launchers: Option<bool>,
    pub events: Option<BuddyEventBus>,
    pub notifier: Option<Arc<dyn TaskNotifier>>,
}

type TerminalCallback = Arc<dyn Fn(&str) + Send + Sync>;

/// The buddy task runner. Cheap to share behind an `Arc`; all mutable hooks
/// are internally synchronized.
pub struct BuddyRunner {
    store: Arc<BuddyStore>,
    execute_launchers: bool,
    events: Option<BuddyEventBus>,
    notifier: Option<Arc<dyn TaskNotifier>>,
    /// Optional callback invoked after a task reaches a terminal-ish state
    /// (DONE / blocking PAUSED / FAILED). Mirrors the mutable
    /// `onTaskTerminal` property of the TS class.
    on_task_terminal: Mutex<Option<TerminalCallback>>,
}

impl BuddyRunner {
    pub fn new(store: Arc<BuddyStore>, options: RunnerOptions) -> Self {
        BuddyRunner {
            store,
            execute_launchers: options.execute_launchers.unwrap_or(true),
            events: options.events,
            notifier: options.notifier,
            on_task_terminal: Mutex::new(None),
        }
    }

    pub fn store(&self) -> &Arc<BuddyStore> {
        &self.store
    }

    /// Wire the queue-coordinator notification hook (TS: `runner.onTaskTerminal = cb`).
    pub fn set_on_task_terminal<F>(&self, callback: F)
    where
        F: Fn(&str) + Send + Sync + 'static,
    {
        *self.on_task_terminal.lock() = Some(Arc::new(callback));
    }

    fn notify_terminal(&self, workspace_key: &str) {
        if let Some(callback) = self.on_task_terminal.lock().clone() {
            callback(workspace_key);
        }
    }

    /// `startTask`. Returns the `run_id` of the started (or health-check) run.
    ///
    /// Auto-advance chains (next actor after a round, queued-instruction
    /// draining, post-health-check start) are trampolined through this loop
    /// instead of the TS promise recursion, so long round chains cannot
    /// overflow the stack. Errors from an auto-advanced start are swallowed
    /// (TS: the `try/catch` in `completeActor`); errors from a user- or
    /// health-check-initiated start propagate.
    pub async fn start_task(
        &self,
        task_id: &str,
        input: StartTaskInput,
    ) -> Result<String, RunnerError> {
        let mut pending: Option<(StartTaskInput, bool)> = Some((input, false));
        let mut outer_run_id: Option<String> = None;
        while let Some((current, swallow_errors)) = pending.take() {
            match self.start_once(task_id, current).await {
                Ok((run_id, followup)) => {
                    if outer_run_id.is_none() {
                        outer_run_id = Some(run_id);
                    }
                    pending = followup;
                }
                Err(error) => {
                    if swallow_errors {
                        break;
                    }
                    return Err(error);
                }
            }
        }
        Ok(outer_run_id.unwrap_or_default())
    }

    /// One iteration of the TS `startTask`: health-check gate, round-window
    /// gate, RUNNING transition, actor execution. Returns the run id and an
    /// optional follow-up start (with its error-swallow flag).
    async fn start_once(
        &self,
        task_id: &str,
        input: StartTaskInput,
    ) -> Result<(String, Option<(StartTaskInput, bool)>), RunnerError> {
        let workspace_key = input
            .workspace_key
            .clone()
            .filter(|k| !k.is_empty())
            .ok_or_else(|| RunnerError::msg("workspace_key is required"))?;
        let detail = self.store.get_task_detail(task_id, &workspace_key).await?;

        // Health check: on first start (round 0, no sessions, no prior health
        // check), ping both actors. Skipped when an explicit actor is requested
        // or in test mode. Re-runs when the previous attempt failed
        // connectivity (FAILED + health_check).
        if self.execute_launchers
            && input.actor.is_none()
            && needs_health_check(&detail.state, &detail.settings)
        {
            let settings_value = serde_json::to_value(&detail.settings)?;
            let implementer = resolve_implementer_actor(&settings_value);
            let reviewer = next_actor_for_settings(&implementer, &settings_value);
            // Clear the stale failed health_check result so the retry can re-trigger.
            self.store
                .update_task_state(task_id, &workspace_key, |mut state| {
                    state.health_check = None;
                    state.latest_failure = None;
                    state.last_error = None;
                    state
                })
                .await?;
            // The post-health-check implementer start is NOT an auto-advance:
            // its errors propagate (TS: runHealthCheck awaits startTask directly).
            let (health_run_id, followup) = self
                .run_health_check(task_id, &workspace_key, &implementer, &reviewer)
                .await?;
            return Ok((health_run_id, followup.map(|next| (next, false))));
        }

        let global_settings = self.store.read_global_settings().await?;
        let actor = input
            .actor
            .clone()
            .or_else(|| {
                if detail.state.status == TaskStatus::Failed {
                    detail
                        .state
                        .latest_failure
                        .as_ref()
                        .and_then(|f| f.actor.clone())
                        .or_else(|| {
                            detail
                                .state
                                .last_error
                                .as_ref()
                                .and_then(|f| f.actor.clone())
                        })
                } else {
                    None
                }
            })
            .unwrap_or_else(|| {
                if detail.state.next_actor.is_empty() {
                    "claude".to_string()
                } else {
                    detail.state.next_actor.clone()
                }
            });
        let status = match actor.as_str() {
            "claude" => TaskStatus::RunningClaude,
            "codex" => TaskStatus::RunningCodex,
            "cursor" => TaskStatus::RunningCursor,
            "opencode" => TaskStatus::RunningOpencode,
            "kimi" => TaskStatus::RunningKimi,
            other => return Err(RunnerError::msg(format!("Unsupported actor: {other}"))),
        };
        if !can_start_from(&detail.state.status) {
            return Err(RunnerError::msg(format!(
                "Cannot start task from {}",
                detail.state.status.as_str()
            )));
        }
        let max_rounds = global_settings.max_rounds.unwrap_or(9999);
        let rounds_in_window = detail.state.rounds_in_window.unwrap_or(0);
        if max_rounds > 0 && rounds_in_window >= max_rounds {
            if detail.state.status == TaskStatus::Paused {
                // User is explicitly resuming from a round-window pause - reset the window.
                self.store
                    .update_task_state(task_id, &workspace_key, |mut state| {
                        state.rounds_in_window = Some(0);
                        state.updated_at = Some(utc_now());
                        state
                    })
                    .await?;
                self.store
                    .append_task_event(
                        task_id,
                        &workspace_key,
                        EventInput {
                            event_type: "round_window.reset".to_string(),
                            payload: payload(serde_json::json!({
                                "previous_rounds_in_window": rounds_in_window,
                                "max_rounds": max_rounds,
                            })),
                            ..Default::default()
                        },
                    )
                    .await?;
            } else {
                // Auto-start attempted but window is exhausted - pause and wait
                // for manual resume.
                self.store
                    .update_task_state(task_id, &workspace_key, |mut state| {
                        state.status = TaskStatus::Paused;
                        state.active_run = None;
                        state.countdown = None;
                        state.updated_at = Some(utc_now());
                        state
                    })
                    .await?;
                self.store
                    .append_task_event(
                        task_id,
                        &workspace_key,
                        EventInput {
                            event_type: "round_window.paused".to_string(),
                            payload: payload(serde_json::json!({
                                "max_rounds": max_rounds,
                                "rounds_in_window": rounds_in_window,
                            })),
                            ..Default::default()
                        },
                    )
                    .await?;
                return Err(RunnerError::msg(format!(
                    "本次自动推进已达到自动轮次上限。点击“继续”可以再推进 {max_rounds} 轮。"
                )));
            }
        }

        let run_id = new_run_id("run");
        let started_at = utc_now();
        let session_id_before =
            session_id_for_actor(&actor, &detail.state, Some(&detail.settings));

        self.store
            .update_task_state(task_id, &workspace_key, |mut state| {
                state.status = status.clone();
                state.active_run = Some(ActiveRun {
                    run_id: Some(run_id.clone()),
                    actor: actor.clone(),
                    started_at: started_at.clone(),
                    status: Some("running".to_string()),
                    session_id_before: session_id_before.clone(),
                    session_id_after: None,
                });
                state.countdown = None;
                state.latest_failure = None;
                state.last_error = None;
                state.updated_at = Some(started_at.clone());
                state
            })
            .await?;
        self.store
            .append_task_event(
                task_id,
                &workspace_key,
                EventInput {
                    event_type: "actor.started".to_string(),
                    actor: Some(actor.clone()),
                    run_id: Some(run_id.clone()),
                    payload: payload(serde_json::json!({
                        "run_id": run_id,
                        "mode": if session_id_before.is_some() { "resume" } else { "start" },
                    })),
                    ..Default::default()
                },
            )
            .await?;

        if !self.execute_launchers {
            return Ok((run_id, None));
        }

        // Follow-up starts produced by complete_actor are auto-advances: their
        // errors are swallowed (TS: try/catch around the auto-start).
        let advance = self
            .execute_actor(
                task_id,
                &workspace_key,
                &actor,
                &run_id,
                input.message.clone().unwrap_or_default(),
            )
            .await?;
        Ok((run_id, advance.map(|next| (next, true))))
    }

    /// `sendMessage`: append a human transcript entry + event, then start a run.
    pub async fn send_message(
        &self,
        task_id: &str,
        input: SendMessageInput,
    ) -> Result<(), RunnerError> {
        let workspace_key = input
            .workspace_key
            .clone()
            .filter(|k| !k.is_empty())
            .ok_or_else(|| RunnerError::msg("workspace_key is required"))?;
        let message = input.message.clone().unwrap_or_default();
        if message.trim().is_empty() {
            return Err(RunnerError::msg("message is required"));
        }
        let mut meta = Map::new();
        meta.insert("source".to_string(), Value::from("run_once"));
        if let Some(attachments) = &input.attachment_meta {
            if !attachments.is_empty() {
                meta.insert("attachments".to_string(), serde_json::to_value(attachments)?);
            }
        }
        self.store
            .append_transcript(task_id, &workspace_key, "human", &message, meta)
            .await?;
        self.store
            .append_task_event(
                task_id,
                &workspace_key,
                EventInput {
                    event_type: "human.message".to_string(),
                    actor: input.actor.clone(),
                    payload: payload(serde_json::json!({ "content": message })),
                    ..Default::default()
                },
            )
            .await?;
        self.start_task(
            task_id,
            StartTaskInput {
                workspace_key: Some(workspace_key),
                actor: input.actor.clone(),
                message: Some(message),
            },
        )
        .await?;
        Ok(())
    }

    /// `pauseCountdown`: COUNTDOWN → READY, marking the countdown paused.
    pub async fn pause_countdown(
        &self,
        task_id: &str,
        input: CountdownInput,
    ) -> Result<(), RunnerError> {
        let workspace_key = input
            .workspace_key
            .clone()
            .filter(|k| !k.is_empty())
            .ok_or_else(|| RunnerError::msg("workspace_key is required"))?;
        let detail = self.store.get_task_detail(task_id, &workspace_key).await?;
        if detail.state.status != TaskStatus::Countdown {
            return Ok(());
        }
        let actor = input
            .next_actor
            .clone()
            .filter(|a| !a.is_empty())
            .or_else(|| {
                if detail.state.next_actor.is_empty() {
                    None
                } else {
                    Some(detail.state.next_actor.clone())
                }
            })
            .or_else(|| {
                detail
                    .state
                    .countdown
                    .as_ref()
                    .map(|c| c.default_next_actor.clone())
                    .filter(|a| !a.is_empty())
            })
            .unwrap_or_else(|| "claude".to_string());
        self.store
            .update_task_state(task_id, &workspace_key, |mut state| {
                let countdown = state.countdown.take().unwrap_or(Countdown {
                    status: "running".to_string(),
                    remaining: Some(0),
                    started_at: None,
                    after_actor: None,
                    default_next_actor: actor.clone(),
                    deadline: None,
                });
                state.status = TaskStatus::Ready;
                state.next_actor = actor.clone();
                state.countdown = Some(Countdown {
                    status: "paused".to_string(),
                    ..countdown
                });
                state.updated_at = Some(utc_now());
                state
            })
            .await?;
        self.store
            .append_task_event(
                task_id,
                &workspace_key,
                EventInput {
                    event_type: "countdown.paused".to_string(),
                    payload: payload(serde_json::json!({ "next_actor": actor })),
                    ..Default::default()
                },
            )
            .await?;
        Ok(())
    }

    /// `skipCountdown`: COUNTDOWN → READY, then immediately start the actor.
    pub async fn skip_countdown(
        &self,
        task_id: &str,
        input: CountdownInput,
    ) -> Result<String, RunnerError> {
        let workspace_key = input
            .workspace_key
            .clone()
            .filter(|k| !k.is_empty())
            .ok_or_else(|| RunnerError::msg("workspace_key is required"))?;
        let detail = self.store.get_task_detail(task_id, &workspace_key).await?;
        if detail.state.status != TaskStatus::Countdown {
            return Err(RunnerError::msg(format!(
                "当前任务不在倒计时中：{task_id}"
            )));
        }
        let actor = input
            .next_actor
            .clone()
            .filter(|a| !a.is_empty())
            .or_else(|| {
                if detail.state.next_actor.is_empty() {
                    None
                } else {
                    Some(detail.state.next_actor.clone())
                }
            })
            .or_else(|| {
                detail
                    .state
                    .countdown
                    .as_ref()
                    .map(|c| c.default_next_actor.clone())
                    .filter(|a| !a.is_empty())
            })
            .ok_or_else(|| RunnerError::msg("next actor is required"))?;
        self.store
            .update_task_state(task_id, &workspace_key, |mut state| {
                state.status = TaskStatus::Ready;
                state.next_actor = actor.clone();
                state.countdown = state.countdown.take().map(|countdown| Countdown {
                    status: "skipped".to_string(),
                    ..countdown
                });
                state.updated_at = Some(utc_now());
                state
            })
            .await?;
        self.store
            .append_task_event(
                task_id,
                &workspace_key,
                EventInput {
                    event_type: "countdown.skipped".to_string(),
                    payload: payload(serde_json::json!({ "next_actor": actor })),
                    ..Default::default()
                },
            )
            .await?;
        self.start_task(
            task_id,
            StartTaskInput {
                workspace_key: Some(workspace_key),
                actor: Some(actor),
                message: None,
            },
        )
        .await
    }

    /// `interrupt`: move the task to PAUSED and clear `active_run`. The
    /// in-flight process is not killed (see module docs); the orphaned run's
    /// completion is ignored by the `active_run.run_id` guards.
    pub async fn interrupt(&self, task_id: &str, workspace_key: &str) -> Result<(), RunnerError> {
        self.store
            .update_task_state(task_id, workspace_key, |mut state| {
                state.status = TaskStatus::Paused;
                state.active_run = None;
                state.updated_at = Some(utc_now());
                state
            })
            .await?;
        self.store
            .append_task_event(
                task_id,
                workspace_key,
                EventInput {
                    event_type: "actor.interrupted".to_string(),
                    ..Default::default()
                },
            )
            .await?;
        Ok(())
    }

    /// `interruptAndInsert`: pull an instruction out of the queue, interrupt
    /// the current actor, then send the instruction as a human message.
    pub async fn interrupt_and_insert(
        &self,
        task_id: &str,
        workspace_key: &str,
        queue_item_id: &str,
    ) -> Result<(), RunnerError> {
        let state = self.store.read_task_state(task_id, workspace_key).await?;
        let item = state
            .instruction_queue
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .find(|i| i.id == queue_item_id)
            .cloned()
            .ok_or_else(|| RunnerError::msg("Instruction not found in queue"))?;
        self.store
            .dequeue_instruction(task_id, workspace_key, queue_item_id)
            .await?;
        self.store
            .update_task_state(task_id, workspace_key, |mut s| {
                s.status = TaskStatus::Paused;
                s.active_run = None;
                s.updated_at = Some(utc_now());
                s
            })
            .await?;
        self.store
            .append_task_event(
                task_id,
                workspace_key,
                EventInput {
                    event_type: "actor.interrupted".to_string(),
                    payload: payload(serde_json::json!({
                        "reason": "interrupt_and_insert",
                        "instruction_id": queue_item_id,
                    })),
                    ..Default::default()
                },
            )
            .await?;
        self.send_message(
            task_id,
            SendMessageInput {
                workspace_key: Some(workspace_key.to_string()),
                message: Some(item.content),
                attachment_meta: item.attachments,
                ..Default::default()
            },
        )
        .await
    }

    pub async fn enqueue_instruction(
        &self,
        task_id: &str,
        workspace_key: &str,
        content: &str,
        attachments: Option<Vec<AttachmentMeta>>,
    ) -> Result<InstructionQueueItem, RunnerError> {
        Ok(self
            .store
            .enqueue_instruction(task_id, workspace_key, content, attachments)
            .await?)
    }

    pub async fn dequeue_instruction(
        &self,
        task_id: &str,
        workspace_key: &str,
        item_id: &str,
    ) -> Result<(), RunnerError> {
        Ok(self
            .store
            .dequeue_instruction(task_id, workspace_key, item_id)
            .await?)
    }

    pub async fn clear_instruction_queue(
        &self,
        task_id: &str,
        workspace_key: &str,
    ) -> Result<(), RunnerError> {
        Ok(self
            .store
            .clear_instruction_queue(task_id, workspace_key)
            .await?)
    }

    /// Startup recovery (TS `BuddyCoreService.recoverInterruptedRuns`): every
    /// task left in RUNNING_*/PINGING is moved to PAUSED with an
    /// `actor.interrupted` event (persisted and published). The queue
    /// coordinator's `rebuild_and_reconcile_all` is invoked by the service
    /// layer after this returns.
    pub async fn recover_interrupted_runs(&self) -> Result<(), RunnerError> {
        for task in self.store.get_tasks().await {
            if task.status.is_running() {
                let event = self
                    .store
                    .append_task_event(
                        &task.task_id,
                        &task.workspace_key,
                        EventInput {
                            event_type: "actor.interrupted".to_string(),
                            payload: payload(serde_json::json!({ "reason": "app_restarted" })),
                            ..Default::default()
                        },
                    )
                    .await?;
                self.store
                    .update_task_state(&task.task_id, &task.workspace_key, |mut state| {
                        state.status = TaskStatus::Paused;
                        state.active_run = None;
                        state.updated_at = Some(utc_now());
                        state
                    })
                    .await?;
                if let Some(events) = &self.events {
                    events.publish(TaskEventEnvelope {
                        workspace_key: task.workspace_key.clone(),
                        task_id: task.task_id.clone(),
                        event,
                    });
                }
            }
        }
        Ok(())
    }

    /// Run an actor command, using a PTY when required (e.g. opencode needs a
    /// TTY). Centralizes the `run_launcher` vs `run_launcher_with_pty`
    /// decision, port of the private `runActorCommand`.
    async fn run_actor_command(
        &self,
        command: &LauncherCommand,
        cwd: &str,
        env: &HashMap<String, String>,
        timeout_ms: u64,
        actor: &str,
        workspace_key: &str,
        task_id: &str,
        run_id: &str,
        output_lines: Arc<Mutex<Vec<String>>>,
        stderr_lines: Arc<Mutex<Vec<String>>>,
    ) -> Result<LauncherRunResult, LauncherError> {
        let parser_actor = parser_actor_for_kind(actor, command.kind);
        let mut merged_env = env.clone();
        if let Some(command_env) = &command.env {
            merged_env.extend(command_env.clone());
        }

        // Streams one parsed text chunk to the event bus as `actor.stdout`.
        let publish_stdout = |text: String| {
            if let Some(events) = &self.events {
                events.publish(TaskEventEnvelope {
                    workspace_key: workspace_key.to_string(),
                    task_id: task_id.to_string(),
                    event: Event {
                        seq: 0,
                        task_id: Some(task_id.to_string()),
                        event_type: "actor.stdout".to_string(),
                        actor: Some(actor.to_string()),
                        ts: utc_now(),
                        run_id: Some(run_id.to_string()),
                        payload: payload(serde_json::json!({ "text": text })),
                    },
                });
            }
        };

        if kind_needs_pty(command.kind) {
            // Expose the osascript shim so opencode plugin notifications
            // (osascript display notification) cannot block the run.
            if let Some(shim_dir) = ensure_osascript_shim_dir(&self.store.data_root).await {
                prepend_path(&mut merged_env, &shim_dir);
            }
            let parser_actor = parser_actor.clone();
            let output_lines = output_lines.clone();
            return run_launcher_with_pty(
                &PtyRunInput {
                    command: command.command.clone(),
                    args: command.args.clone(),
                    cwd: cwd.to_string(),
                    env: Some(merged_env),
                    timeout_ms,
                    abort: None,
                },
                move |data| {
                    for line in data
                        .split('\n')
                        .map(|l| l.strip_suffix('\r').unwrap_or(l))
                        .filter(|l| !l.is_empty())
                    {
                        output_lines.lock().push(line.to_string());
                        if let Ok(parsed) = parse_actor_line(&parser_actor, line) {
                            if let Some(text) = parsed.text {
                                publish_stdout(text);
                            }
                        }
                    }
                },
            )
            .await;
        }

        let parser_actor = parser_actor.clone();
        let stdout_lines = output_lines;
        run_launcher(
            &RunLauncherInput {
                command: command.command.clone(),
                args: command.args.clone(),
                cwd: cwd.to_string(),
                env: Some(merged_env),
                stdin_text: command.stdin_text.clone(),
                timeout_ms,
                abort: None,
            },
            move |line| {
                stdout_lines.lock().push(line.clone());
                if let Ok(parsed) = parse_actor_line(&parser_actor, &line) {
                    if let Some(text) = parsed.text {
                        publish_stdout(text);
                    }
                }
            },
            move |line| {
                stderr_lines.lock().push(line);
            },
        )
        .await
    }

    /// `executePing`: connectivity ping with upgrade-exit auto-retry.
    async fn execute_ping(
        &self,
        task_id: &str,
        workspace_key: &str,
        actor: &str,
    ) -> Result<PingOutcome, RunnerError> {
        let global_settings = self.store.read_global_settings().await?;
        let max_upgrade_retries = global_settings
            .max_upgrade_retries
            .unwrap_or(DEFAULT_MAX_UPGRADE_RETRIES);

        let mut upgrade_retries = 0u32;
        loop {
            let attempt = self.execute_ping_attempt(task_id, workspace_key, actor).await?;
            if attempt.success {
                return Ok(attempt);
            }

            let combined = format!(
                "{}\n{}\n{}",
                attempt.stderr, attempt.stdout,
                attempt.error.as_deref().unwrap_or("")
            )
            .trim()
            .to_string();
            if upgrade_retries < max_upgrade_retries && is_upgrade_exit_error(&combined) {
                upgrade_retries += 1;
                self.store
                    .append_task_event(
                        task_id,
                        workspace_key,
                        EventInput {
                            event_type: "health_check.actor_upgrade_retry".to_string(),
                            actor: Some(actor.to_string()),
                            payload: payload(serde_json::json!({
                                "retry_attempt": upgrade_retries,
                                "max_retries": max_upgrade_retries,
                                "error": truncate_chars(attempt.error.as_deref().unwrap_or(""), 500),
                            })),
                            ..Default::default()
                        },
                    )
                    .await?;
                let mut meta = Map::new();
                meta.insert(
                    "kind".to_string(),
                    Value::from("health_check_upgrade_retry"),
                );
                meta.insert("retry_attempt".to_string(), Value::from(upgrade_retries));
                meta.insert("actor".to_string(), Value::from(actor));
                self.store
                    .append_transcript(
                        task_id,
                        workspace_key,
                        "system",
                        &format!(
                            "{} 连通性检查检测到自动升级，等待升级完成后重试 ({upgrade_retries}/{max_upgrade_retries})...",
                            actor_display_name(actor)
                        ),
                        meta,
                    )
                    .await?;
                tokio::time::sleep(std::time::Duration::from_millis(UPGRADE_WAIT_MS)).await;
                continue;
            }
            return Ok(attempt);
        }
    }

    /// `executePingAttempt`: a single connectivity ping. Store/fs setup errors
    /// propagate (TS: they reject the promise); run failures are captured in
    /// the returned outcome (TS: try/catch around the run).
    async fn execute_ping_attempt(
        &self,
        task_id: &str,
        workspace_key: &str,
        actor: &str,
    ) -> Result<PingOutcome, RunnerError> {
        let detail = self.store.get_task_detail(task_id, workspace_key).await?;
        let launcher = detail
            .settings
            .launchers
            .get(actor)
            .cloned()
            .unwrap_or(Launcher {
                command: actor.to_string(),
                env: HashMap::new(),
                timeout_seconds: PING_TIMEOUT_SECONDS,
            });
        let task_directory = self.store.task_directory(task_id, workspace_key);
        let artifacts_dir = task_directory.join("artifacts");
        tokio::fs::create_dir_all(&artifacts_dir).await?;

        let run_id = new_run_id("ping");
        let prompt = build_ping_prompt(actor);
        let prompt_file = artifacts_dir.join(format!("{run_id}-prompt.md"));
        let output_file = artifacts_dir.join(format!("{run_id}-output.md"));
        let event_file = artifacts_dir.join(format!("{run_id}-events.jsonl"));
        tokio::fs::write(&prompt_file, &prompt).await?;

        let cwd = existing_cwd(detail.state.repo_root.as_deref()).await;
        let command = build_launcher_command(&LauncherCommandInput {
            actor: actor.to_string(),
            command: launcher.command.clone(),
            mode: Some("start".to_string()),
            prompt_file: prompt_file.to_string_lossy().to_string(),
            prompt_text: Some(prompt),
            event_file: Some(event_file.to_string_lossy().to_string()),
            output_file: Some(output_file.to_string_lossy().to_string()),
            repo_root: Some(cwd.clone()),
            task_dir: Some(task_directory.to_string_lossy().to_string()),
            run_id: Some(run_id.clone()),
            session_id: None,
        });

        let output_lines = Arc::new(Mutex::new(Vec::<String>::new()));
        let stderr_lines = Arc::new(Mutex::new(Vec::<String>::new()));

        let result = self
            .run_actor_command(
                &command,
                &cwd,
                &launcher.env,
                PING_TIMEOUT_SECONDS * 1000,
                actor,
                workspace_key,
                task_id,
                &run_id,
                output_lines.clone(),
                stderr_lines.clone(),
            )
            .await;

        let stdout_text = output_lines.lock().join("\n");
        let stderr_text = stderr_lines.lock().join("\n").trim().to_string();

        match result {
            Ok(result) => {
                let raw_events =
                    collect_raw_events(&event_file, &stdout_text, command.kind).await;
                let output_text =
                    collect_output_text(actor, command.kind, &output_file, &stdout_text).await;
                let parsed_lines = parse_actor_events(
                    &parser_actor_for_kind(actor, command.kind),
                    &raw_events,
                );

                if result.exit_code != Some(0) {
                    let error = if !stderr_text.is_empty() {
                        stderr_text.clone()
                    } else if !output_text.trim().is_empty() {
                        output_text.trim().to_string()
                    } else {
                        exit_error_message(result.exit_code, result.signal.as_deref())
                    };
                    return Ok(PingOutcome {
                        success: false,
                        error: Some(truncate_chars(&error, 300)),
                        stderr: stderr_text,
                        stdout: stdout_text,
                        ..Default::default()
                    });
                }

                // Verify the actor responded with a valid buddy message.
                let message = parse_buddy_message(&output_text);
                let has_content = match &message {
                    BuddyMessage::Message { text } => !text.trim().is_empty(),
                    BuddyMessage::Break { content, .. } => !content.trim().is_empty(),
                };
                if !has_content {
                    return Ok(PingOutcome {
                        success: false,
                        error: Some("Actor responded with empty content".to_string()),
                        stderr: stderr_text,
                        stdout: stdout_text,
                        ..Default::default()
                    });
                }

                let session_id = last_value(parsed_lines.iter().map(|l| l.session_id.clone()));
                let thread_id = last_value(parsed_lines.iter().map(|l| l.thread_id.clone()));
                Ok(PingOutcome {
                    success: true,
                    session_id,
                    thread_id,
                    stderr: stderr_text,
                    stdout: stdout_text,
                    ..Default::default()
                })
            }
            Err(error) => {
                let message = error.to_string();
                let is_only_warning =
                    !stderr_text.is_empty() && is_cli_warning_only(&stderr_text);
                let fallback = if !is_only_warning {
                    stderr_text.clone()
                } else {
                    "Actor exited without producing any output".to_string()
                };
                Ok(PingOutcome {
                    success: false,
                    error: Some(truncate_chars(
                        if message.is_empty() { &fallback } else { &message },
                        300,
                    )),
                    stderr: stderr_text,
                    stdout: stdout_text,
                    ..Default::default()
                })
            }
        }
    }

    /// `runHealthCheck`: ping implementer + reviewer concurrently, then either
    /// transition to READY and hand back the implementer start as a follow-up
    /// (trampolined by `start_task`), or fail the task. Returns the run id the
    /// TS original fabricates for the health check plus the follow-up start.
    async fn run_health_check(
        &self,
        task_id: &str,
        workspace_key: &str,
        implementer: &str,
        reviewer: &str,
    ) -> Result<(String, Option<StartTaskInput>), RunnerError> {
        let actors = vec![implementer.to_string(), reviewer.to_string()];
        let mut pending_results = HashMap::new();
        for actor in &actors {
            pending_results.insert(actor.clone(), "pending".to_string());
        }

        self.store
            .update_task_state(task_id, workspace_key, |mut state| {
                state.status = TaskStatus::Pinging;
                state.health_check = Some(HealthCheckResult {
                    actors: pending_results.clone(),
                    failed_actor: None,
                    failed_reason: None,
                });
                state.updated_at = Some(utc_now());
                state
            })
            .await?;
        self.store
            .append_task_event(
                task_id,
                workspace_key,
                EventInput {
                    event_type: "health_check.started".to_string(),
                    payload: payload(serde_json::json!({ "actors": actors })),
                    ..Default::default()
                },
            )
            .await?;
        let mut meta = Map::new();
        meta.insert("kind".to_string(), Value::from("health_check"));
        meta.insert("actors".to_string(), serde_json::to_value(&actors)?);
        self.store
            .append_transcript(task_id, workspace_key, "system", "health_check.started", meta)
            .await?;

        let mut running_results = pending_results.clone();
        for actor in &actors {
            running_results.insert(actor.clone(), "running".to_string());
        }
        self.store
            .update_task_state(task_id, workspace_key, |mut state| {
                state.health_check = Some(HealthCheckResult {
                    actors: running_results.clone(),
                    failed_actor: None,
                    failed_reason: None,
                });
                state.updated_at = Some(utc_now());
                state
            })
            .await?;

        // TS: Promise.allSettled over both pings (concurrent).
        let (first, second) = tokio::join!(
            self.execute_ping(task_id, workspace_key, &actors[0]),
            self.execute_ping(task_id, workspace_key, &actors[1])
        );
        let ping_results = [first, second];

        let mut all_passed = true;
        let mut failed_actor: Option<String> = None;
        let mut failed_reason: Option<String> = None;
        let mut final_results = running_results.clone();
        // (state field, value) session updates collected from passed actors.
        let mut session_updates: Vec<(&'static str, String)> = Vec::new();

        for (index, actor) in actors.iter().enumerate() {
            let settled = &ping_results[index];
            let outcome = settled.as_ref().ok();
            if settled.is_ok() && outcome.map(|o| o.success).unwrap_or(false) {
                let outcome = outcome.expect("checked above");
                final_results.insert(actor.clone(), "passed".to_string());
                let sid = outcome.session_id.clone();
                let tid = outcome.thread_id.clone();
                match actor.as_str() {
                    "claude" => {
                        if let Some(sid) = &sid {
                            session_updates.push(("claude_session_id", sid.clone()));
                        }
                    }
                    "codex" => {
                        if let Some(id) = tid.clone().or_else(|| sid.clone()) {
                            session_updates.push(("codex_thread_id", id));
                        }
                    }
                    "cursor" => {
                        if let Some(sid) = &sid {
                            session_updates.push(("cursor_session_id", sid.clone()));
                        }
                    }
                    "opencode" => {
                        if let Some(sid) = &sid {
                            session_updates.push(("opencode_session_id", sid.clone()));
                        }
                    }
                    "kimi" => {
                        if let Some(sid) = &sid {
                            session_updates.push(("kimi_session_id", sid.clone()));
                        }
                    }
                    _ => {}
                }
                let display_id = if actor == "codex" {
                    tid.or(sid)
                } else {
                    sid
                };
                self.store
                    .append_task_event(
                        task_id,
                        workspace_key,
                        EventInput {
                            event_type: "health_check.actor_passed".to_string(),
                            actor: Some(actor.clone()),
                            payload: payload(serde_json::json!({ "session_id": display_id })),
                            ..Default::default()
                        },
                    )
                    .await?;
            } else {
                all_passed = false;
                final_results.insert(actor.clone(), "failed".to_string());
                if failed_actor.is_none() {
                    failed_actor = Some(actor.clone());
                    failed_reason = Some(match settled {
                        Ok(outcome) => outcome.error.clone().unwrap_or_default(),
                        Err(error) => error.to_string(),
                    });
                }
                self.store
                    .append_task_event(
                        task_id,
                        workspace_key,
                        EventInput {
                            event_type: "health_check.actor_failed".to_string(),
                            actor: Some(actor.clone()),
                            payload: payload(serde_json::json!({
                                "error": failed_reason.clone().unwrap_or_else(|| "Unknown error".to_string()),
                            })),
                            ..Default::default()
                        },
                    )
                    .await?;
            }
        }

        if all_passed {
            let updates = session_updates.clone();
            self.store
                .update_task_state(task_id, workspace_key, move |mut state| {
                    state.status = TaskStatus::Ready;
                    state.health_check = None;
                    for (key, value) in &updates {
                        set_session_field(&mut state, key, value.clone());
                    }
                    state.updated_at = Some(utc_now());
                    state
                })
                .await?;
            self.store
                .append_task_event(
                    task_id,
                    workspace_key,
                    EventInput {
                        event_type: "health_check.passed".to_string(),
                        ..Default::default()
                    },
                )
                .await?;
            let session_ids: Vec<Value> = actors
                .iter()
                .map(|actor| {
                    let sid = session_updates
                        .iter()
                        .find(|(key, _)| session_field_for_actor(actor) == Some(*key))
                        .map(|(_, value)| value.clone());
                    serde_json::json!({ "actor": actor, "session_id": sid })
                })
                .collect();
            let mut meta = Map::new();
            meta.insert("kind".to_string(), Value::from("health_check"));
            meta.insert("actors".to_string(), serde_json::to_value(&actors)?);
            meta.insert("session_ids".to_string(), Value::Array(session_ids));
            self.store
                .append_transcript(task_id, workspace_key, "system", "health_check.passed", meta)
                .await?;

            if self.execute_launchers {
                let run_id = new_run_id("run");
                return Ok((
                    run_id,
                    Some(StartTaskInput {
                        workspace_key: Some(workspace_key.to_string()),
                        actor: Some(implementer.to_string()),
                        message: None,
                    }),
                ));
            }
            return Ok((
                format!("ping_ok_{}", Utc::now().timestamp_millis()),
                None,
            ));
        }

        let failed_at = utc_now();
        let failure_record = Failure {
            actor: failed_actor.clone(),
            message: format!(
                "连通性检查失败：{} — {}",
                failed_actor
                    .as_deref()
                    .map(actor_display_name)
                    .unwrap_or_else(|| "未知".to_string()),
                failed_reason.as_deref().unwrap_or("未知错误")
            ),
            run_id: None,
            ts: Some(failed_at.clone()),
            output_file: None,
            event_file: None,
        };
        let updates = session_updates.clone();
        let failure_for_state = failure_record.clone();
        let failed_actor_for_state = failed_actor.clone();
        let failed_reason_for_state = failed_reason.clone();
        self.store
            .update_task_state(task_id, workspace_key, move |mut state| {
                state.status = TaskStatus::Failed;
                state.active_run = None;
                state.latest_failure = Some(failure_for_state.clone());
                state.last_error = Some(failure_for_state.clone());
                state.health_check = Some(HealthCheckResult {
                    actors: final_results,
                    failed_actor: failed_actor_for_state,
                    failed_reason: failed_reason_for_state,
                });
                for (key, value) in &updates {
                    set_session_field(&mut state, key, value.clone());
                }
                state.updated_at = Some(failed_at);
                state
            })
            .await?;
        // TS payload: { failed_actor, failed_reason } with undefined keys dropped.
        let mut event_payload = Map::new();
        if let Some(actor) = &failed_actor {
            event_payload.insert("failed_actor".to_string(), Value::from(actor.clone()));
        }
        if let Some(reason) = &failed_reason {
            event_payload.insert("failed_reason".to_string(), Value::from(reason.clone()));
        }
        self.store
            .append_task_event(
                task_id,
                workspace_key,
                EventInput {
                    event_type: "health_check.failed".to_string(),
                    payload: event_payload,
                    ..Default::default()
                },
            )
            .await?;
        let mut meta = Map::new();
        meta.insert("kind".to_string(), Value::from("health_check_failed"));
        meta.insert(
            "failed_actor".to_string(),
            Value::from(failed_actor.clone().unwrap_or_default()),
        );
        meta.insert(
            "failed_reason".to_string(),
            Value::from(failed_reason.clone().unwrap_or_default()),
        );
        self.store
            .append_transcript(task_id, workspace_key, "system", "health_check.failed", meta)
            .await?;
        if let Some(notifier) = &self.notifier {
            notifier
                .notify_task_failed(
                    task_id,
                    workspace_key,
                    failed_actor.as_deref().unwrap_or("unknown"),
                    &format!(
                        "健康检查失败：{}",
                        failed_reason.as_deref().unwrap_or("未知错误")
                    ),
                )
                .await;
        }
        // TS: `actorDisplayName(failedActor)` maps undefined to 'Codex'.
        Err(RunnerError::msg(format!(
            "连通性检查失败：{} — {}",
            actor_display_name(failed_actor.as_deref().unwrap_or("")),
            failed_reason.as_deref().unwrap_or("未知错误")
        )))
    }

    /// `executeActor` — runs the actor with compact/upgrade retry handling.
    /// The TS original recurses; here a loop carries the retry counters.
    /// Returns the follow-up start produced by `completeActor` (auto-advance).
    async fn execute_actor(
        &self,
        task_id: &str,
        workspace_key: &str,
        actor: &str,
        run_id: &str,
        user_message: String,
    ) -> Result<Option<StartTaskInput>, RunnerError> {
        let mut compact_retries = 0u32;
        let mut upgrade_retries = 0u32;
        loop {
            let failure = match self
                .execute_actor_attempt(task_id, workspace_key, actor, run_id, &user_message)
                .await
            {
                Ok(advance) => return Ok(advance),
                Err(AttemptError::Fatal(error)) => return Err(error),
                Err(AttemptError::Run(failure)) => failure,
            };

            let max_compact_retries = failure
                .global_settings
                .max_compact_retries
                .unwrap_or(DEFAULT_MAX_COMPACT_RETRIES);
            // Auto-reset session on context window limit errors. `/compact`
            // does NOT work in -p (pipe) mode, so we go straight to a session
            // reset with an injected compact context.
            if is_context_window_limit_error(&failure.message)
                && compact_retries < max_compact_retries
                && session_id_for_actor(actor, &failure.detail.state, Some(&failure.detail.settings))
                    .is_some()
            {
                self.store
                    .append_task_event(
                        task_id,
                        workspace_key,
                        EventInput {
                            event_type: "actor.context_limit_detected".to_string(),
                            actor: Some(actor.to_string()),
                            run_id: Some(run_id.to_string()),
                            payload: payload(serde_json::json!({
                                "error": failure.message,
                                "reset_attempt": compact_retries + 1,
                                "max_reset_attempts": max_compact_retries,
                            })),
                            ..Default::default()
                        },
                    )
                    .await?;
                let mut meta = Map::new();
                meta.insert("kind".to_string(), Value::from("session_reset"));
                meta.insert(
                    "reset_attempt".to_string(),
                    Value::from(compact_retries + 1),
                );
                self.store
                    .append_transcript(
                        task_id,
                        workspace_key,
                        "system",
                        &format!(
                            "{} 达到上下文窗口限制，正在重置会话并注入精简上下文 ({}/{})...",
                            actor_display_name(actor),
                            compact_retries + 1,
                            max_compact_retries
                        ),
                        meta,
                    )
                    .await?;
                self.reset_session_for_actor(task_id, workspace_key, actor, &failure.detail)
                    .await?;
                compact_retries += 1;
                continue;
            }

            // Auto-retry when the child process exits due to an auto-upgrade
            // (e.g. wecode/codex). Include raw stdout: wecode prints upgrade
            // progress to stdout, which `extractActorOutput` filters out.
            let max_upgrade_retries = failure
                .global_settings
                .max_upgrade_retries
                .unwrap_or(DEFAULT_MAX_UPGRADE_RETRIES);
            let combined_message = format!(
                "{}\n{}\n{}",
                failure.message, failure.stderr_text, failure.stdout_text
            )
            .trim()
            .to_string();
            if upgrade_retries < max_upgrade_retries && is_upgrade_exit_error(&combined_message) {
                self.store
                    .append_task_event(
                        task_id,
                        workspace_key,
                        EventInput {
                            event_type: "actor.upgrade_detected".to_string(),
                            actor: Some(actor.to_string()),
                            run_id: Some(run_id.to_string()),
                            payload: payload(serde_json::json!({
                                "retry_attempt": upgrade_retries + 1,
                                "max_retries": max_upgrade_retries,
                                "error": truncate_chars(&failure.message, 500),
                            })),
                            ..Default::default()
                        },
                    )
                    .await?;
                let mut meta = Map::new();
                meta.insert("kind".to_string(), Value::from("upgrade_retry"));
                meta.insert(
                    "retry_attempt".to_string(),
                    Value::from(upgrade_retries + 1),
                );
                self.store
                    .append_transcript(
                        task_id,
                        workspace_key,
                        "system",
                        &format!(
                            "{} 检测到自动升级，等待升级完成后重试 ({}/{})...",
                            actor_display_name(actor),
                            upgrade_retries + 1,
                            max_upgrade_retries
                        ),
                        meta,
                    )
                    .await?;
                tokio::time::sleep(std::time::Duration::from_millis(UPGRADE_WAIT_MS)).await;
                upgrade_retries += 1;
                continue;
            }

            self.mark_failed(task_id, workspace_key, actor, &failure.message, Some(run_id))
                .await?;
            return Err(RunnerError::msg(failure.message));
        }
    }

    /// One actor execution attempt, port of `executeActorInner`. Setup errors
    /// (detail read, prompt build/write, lock creation) are
    /// [`AttemptError::Fatal`] — in TS they escape the try/catch and skip
    /// `markFailed`. Run errors are [`AttemptError::Run`].
    async fn execute_actor_attempt(
        &self,
        task_id: &str,
        workspace_key: &str,
        actor: &str,
        run_id: &str,
        user_message: &str,
    ) -> Result<Option<StartTaskInput>, AttemptError> {
        let detail = self
            .store
            .get_task_detail(task_id, workspace_key)
            .await
            .map_err(|e| AttemptError::Fatal(e.into()))?;
        let global_settings = self
            .store
            .read_global_settings()
            .await
            .map_err(|e| AttemptError::Fatal(e.into()))?;
        let launcher = detail
            .settings
            .launchers
            .get(actor)
            .cloned()
            .unwrap_or(Launcher {
                command: actor.to_string(),
                env: HashMap::new(),
                timeout_seconds: 600,
            });
        let task_directory = self.store.task_directory(task_id, workspace_key);
        let artifacts_dir = task_directory.join("artifacts");
        tokio::fs::create_dir_all(&artifacts_dir)
            .await
            .map_err(|e| AttemptError::Fatal(e.into()))?;
        let prompt = build_actor_prompt(&BuildActorPromptInput {
            actor: actor.to_string(),
            round: detail.state.round,
            repo_root: detail.state.repo_root.clone().unwrap_or_default(),
            task_text: detail.task_text.clone(),
            context_text: detail.context_text.clone(),
            transcript: detail.transcript.iter().map(TranscriptRow::from).collect(),
            settings: Some(
                serde_json::to_value(&detail.settings).map_err(|e| AttemptError::Fatal(e.into()))?,
            ),
            state: Some(
                serde_json::to_value(&detail.state).map_err(|e| AttemptError::Fatal(e.into()))?,
            ),
            global_settings: Some(global_settings.clone()),
            user_message: Some(user_message.to_string()),
        });
        let prompt_file = artifacts_dir.join(format!("{run_id}-prompt.md"));
        let output_file = artifacts_dir.join(format!("{run_id}-output.md"));
        let event_file = artifacts_dir.join(format!("{run_id}-events.jsonl"));
        tokio::fs::write(&prompt_file, &prompt)
            .await
            .map_err(|e| AttemptError::Fatal(e.into()))?;
        let cwd = existing_cwd(detail.state.repo_root.as_deref()).await;
        let existing_session_id =
            session_id_for_actor(actor, &detail.state, Some(&detail.settings));
        // TS: `actor === 'kimi' && commandKind === 'native_kimi' ? existingSessionId
        // : (existingSessionId ?? undefined)` — both branches are the same value.
        let _command_kind = command_kind_for(actor, &launcher.command);
        let session_id = existing_session_id.clone();
        let command = build_launcher_command(&LauncherCommandInput {
            actor: actor.to_string(),
            command: launcher.command.clone(),
            mode: Some(
                if existing_session_id.is_some() {
                    "resume"
                } else {
                    "start"
                }
                .to_string(),
            ),
            prompt_file: prompt_file.to_string_lossy().to_string(),
            prompt_text: Some(prompt),
            event_file: Some(event_file.to_string_lossy().to_string()),
            output_file: Some(output_file.to_string_lossy().to_string()),
            repo_root: Some(cwd.clone()),
            task_dir: Some(task_directory.to_string_lossy().to_string()),
            run_id: Some(run_id.to_string()),
            session_id: session_id.clone(),
        });
        let output_lines = Arc::new(Mutex::new(Vec::<String>::new()));
        let stderr_lines = Arc::new(Mutex::new(Vec::<String>::new()));
        let lock_path = create_run_lock(
            &self.store.data_root,
            workspace_key,
            task_id,
            run_id,
            std::process::id(),
        )
        .await
        .map_err(|e| AttemptError::Fatal(e.into()))?;

        // Everything below runs inside the TS try/catch/finally.
        let run_result: Result<Option<StartTaskInput>, RunnerError> = async {
            let started = std::time::Instant::now();
            let result = self
                .run_actor_command(
                    &command,
                    &cwd,
                    &launcher.env,
                    launcher.timeout_seconds.saturating_mul(1000),
                    actor,
                    workspace_key,
                    task_id,
                    run_id,
                    output_lines.clone(),
                    stderr_lines.clone(),
                )
                .await?;
            let elapsed_ms = started.elapsed().as_millis() as u64;

            let stdout_text = output_lines.lock().join("\n");
            let raw_events = collect_raw_events(&event_file, &stdout_text, command.kind).await;
            let mut output_text =
                collect_output_text(actor, command.kind, &output_file, &stdout_text).await;
            let mut parsed_lines =
                parse_actor_events(&parser_actor_for_kind(actor, command.kind), &raw_events);
            if actor == "kimi"
                && session_id.is_some()
                && !parsed_lines.iter().any(|line| line.session_id.is_some())
            {
                parsed_lines.push(ParsedActorLine {
                    session_id: session_id.clone(),
                    ..Default::default()
                });
            }

            let error_only_output = parsed_lines
                .iter()
                .any(|l| l.raw_type.as_deref() == Some("error"))
                && !parsed_lines
                    .iter()
                    .any(|l| l.raw_type.as_deref() != Some("error") && l.text.is_some());
            if result.exit_code != Some(0) || error_only_output {
                let mut parts: Vec<String> = Vec::new();
                let stderr_text = stderr_lines.lock().join("\n").trim().to_string();
                if !stderr_text.is_empty() {
                    parts.push(stderr_text);
                }
                let event_error = parsed_lines
                    .iter()
                    .filter(|l| l.raw_type.as_deref() == Some("error") && l.text.is_some())
                    .filter_map(|l| l.text.clone())
                    .collect::<Vec<_>>()
                    .join("\n");
                if !event_error.is_empty() {
                    parts.push(event_error);
                }
                if !output_text.trim().is_empty() {
                    parts.push(output_text.trim().to_string());
                }
                return Err(RunnerError::msg(if parts.is_empty() {
                    exit_error_message(result.exit_code, result.signal.as_deref())
                } else {
                    parts.join("\n\n")
                }));
            }

            // Ghost output: raw events exist but nothing was extracted
            // (unrecognized error format). Skip noise events (system/hook/
            // step_start/step_finish) that carry no actor content.
            let non_noise_events: Vec<String> = raw_events
                .trim()
                .split('\n')
                .map(|l| l.strip_suffix('\r').unwrap_or(l))
                .filter(|line| {
                    if line.trim().is_empty() {
                        return false;
                    }
                    match serde_json::from_str::<Value>(line) {
                        Ok(obj) => {
                            let event_type = obj.get("type").and_then(Value::as_str);
                            let subtype = obj.get("subtype");
                            if event_type == Some("system")
                                && subtype
                                    .and_then(Value::as_str)
                                    .map(|s| s.starts_with("hook_"))
                                    .unwrap_or(false)
                            {
                                return false;
                            }
                            if event_type == Some("system") && subtype.is_some() {
                                return false;
                            }
                            if event_type == Some("step_start") {
                                return false;
                            }
                            if event_type == Some("step_finish") {
                                return false;
                            }
                            true
                        }
                        Err(_) => true, // keep non-JSON lines
                    }
                })
                .map(str::to_string)
                .collect();
            let non_noise_raw = non_noise_events.join("\n");
            let non_noise_parsed_text = parsed_lines
                .iter()
                .filter(|l| l.text.is_some() && !l.noise)
                .filter_map(|l| l.text.clone())
                .collect::<Vec<_>>()
                .join("\n")
                .trim()
                .to_string();
            let has_only_noise_output = output_text.trim().is_empty()
                && non_noise_parsed_text.is_empty()
                && parsed_lines.iter().any(|l| l.noise && l.text.is_some());
            if has_only_noise_output {
                return Err(RunnerError::msg(format!(
                    "Actor exited with only noise events (likely {CONTEXT_EXHAUSTED_PHRASE})"
                )));
            } else if output_text.trim().is_empty() && !non_noise_raw.trim().is_empty() {
                let parsed_text = if !non_noise_parsed_text.is_empty() {
                    non_noise_parsed_text
                } else {
                    parsed_lines
                        .iter()
                        .filter_map(|l| l.text.clone())
                        .collect::<Vec<_>>()
                        .join("\n")
                        .trim()
                        .to_string()
                };
                if !parsed_text.is_empty() {
                    output_text = parsed_text;
                } else {
                    return Err(RunnerError::msg(truncate_chars(non_noise_raw.trim(), 500)));
                }
            } else if output_text.trim().is_empty()
                && !raw_events.trim().is_empty()
                && non_noise_raw.trim().is_empty()
            {
                // All events were noise (e.g. only system/hook events).
                return Err(RunnerError::msg(
                    "Actor exited without producing any output",
                ));
            }

            self.complete_actor(
                task_id,
                workspace_key,
                actor,
                run_id,
                &output_text,
                &parsed_lines,
                elapsed_ms,
                result.exit_code.unwrap_or(0),
            )
            .await
        }
        .await;

        let stderr_text = stderr_lines.lock().join("\n").trim().to_string();
        let stdout_text = output_lines.lock().join("\n");
        remove_run_lock(&lock_path).await.ok();

        match run_result {
            Ok(advance) => Ok(advance),
            Err(error) => {
                // Prefer the actual error message over stderr; only use stderr
                // as fallback and filter out known CLI warnings.
                let message = error.to_string();
                let is_only_warning =
                    !stderr_text.is_empty() && is_cli_warning_only(&stderr_text);
                let failure_message = if !message.is_empty() {
                    message
                } else if !is_only_warning {
                    stderr_text.clone()
                } else {
                    "Actor exited without producing any output".to_string()
                };
                Err(AttemptError::Run(ActorRunFailure {
                    message: failure_message,
                    stderr_text,
                    stdout_text,
                    detail,
                    global_settings,
                }))
            }
        }
    }

    /// `completeActor`: record the round, advance the state machine
    /// (dual-break, round window), and produce the auto-advance follow-up
    /// start (executed by the `start_task` trampoline, not recursively).
    #[allow(clippy::too_many_arguments)]
    async fn complete_actor(
        &self,
        task_id: &str,
        workspace_key: &str,
        actor: &str,
        run_id: &str,
        output_text: &str,
        parsed_lines: &[ParsedActorLine],
        elapsed_ms: u64,
        exit_code: i32,
    ) -> Result<Option<StartTaskInput>, RunnerError> {
        // Guard: if the task was interrupted (active_run cleared or changed),
        // skip completion.
        let current_state = self.store.read_task_state(task_id, workspace_key).await?;
        if current_state
            .active_run
            .as_ref()
            .and_then(|r| r.run_id.as_deref())
            != Some(run_id)
        {
            return Ok(None);
        }

        let text = output_text;
        let session_id = last_value(parsed_lines.iter().map(|l| l.session_id.clone()));
        let thread_id = last_value(parsed_lines.iter().map(|l| l.thread_id.clone()));
        let message = parse_buddy_message(text);

        // Degraded response detection: only noise placeholders with no buddy
        // protocol JSON → treat as a context window limit error.
        let has_non_noise_content = parsed_lines.iter().any(|l| l.text.is_some() && !l.noise);
        let has_buddy_json_in_output = match &message {
            BuddyMessage::Message { text: message_text } => message_text != text,
            BuddyMessage::Break { .. } => true,
        };
        let is_degraded_response = !has_non_noise_content
            && !has_buddy_json_in_output
            && matches!(message, BuddyMessage::Message { .. });
        if is_degraded_response {
            return Err(RunnerError::msg(format!(
                "Actor produced only noise events ({CONTEXT_EXHAUSTED_PHRASE} likely): {}",
                truncate_chars(text, 200)
            )));
        }

        let detail = self.store.get_task_detail(task_id, workspace_key).await?;
        let global_settings = self.store.read_global_settings().await?;
        let settings_value = serde_json::to_value(&detail.settings)?;
        let next_actor = next_actor_for_settings(actor, &settings_value);
        let round = detail.state.round + 1;
        let rounds_in_window = detail.state.rounds_in_window.unwrap_or(0) + 1;
        let max_rounds = global_settings.max_rounds.unwrap_or(9999);
        let round_window_reached = max_rounds > 0 && rounds_in_window >= max_rounds;
        let now = utc_now();
        let (buddy_type, transcript_content) = match &message {
            BuddyMessage::Break { content, .. } => ("break", content.clone()),
            BuddyMessage::Message { text } => ("chat", text.clone()),
        };
        let pending_break = detail.state.pending_break.clone();
        let is_break = matches!(message, BuddyMessage::Break { .. });
        let break_confirmed = is_break
            && pending_break
                .as_ref()
                .and_then(|p| p.actor.as_deref())
                .map(|a| a != actor)
                .unwrap_or(false);
        let break_pending = is_break && !break_confirmed;
        let break_rejected =
            !is_break && pending_break.as_ref().and_then(|p| p.actor.as_deref()).is_some();
        let has_queued_instructions = current_state
            .instruction_queue
            .as_deref()
            .map(|q| !q.is_empty())
            .unwrap_or(false);

        let mut transcript_meta = Map::new();
        transcript_meta.insert("round".to_string(), Value::from(round));
        transcript_meta.insert("run_id".to_string(), Value::from(run_id));
        transcript_meta.insert("elapsed_ms".to_string(), Value::from(elapsed_ms));
        transcript_meta.insert("buddy_type".to_string(), Value::from(buddy_type));
        self.store
            .append_transcript(
                task_id,
                workspace_key,
                normalize_actor_role(actor),
                &transcript_content,
                transcript_meta,
            )
            .await?;
        self.store
            .append_task_event(
                task_id,
                workspace_key,
                EventInput {
                    event_type: "actor.completed".to_string(),
                    actor: Some(actor.to_string()),
                    run_id: Some(run_id.to_string()),
                    payload: payload(serde_json::json!({
                        "run_id": run_id,
                        "text": transcript_content,
                        "raw_text": text,
                        "buddy_type": buddy_type,
                    })),
                    ..Default::default()
                },
            )
            .await?;

        let context_hash = hash_text(&detail.context_text);
        let pending_break_for_update = pending_break.clone();
        self.store
            .update_task_state(task_id, workspace_key, |mut state| {
                let mut context_sent = state.context_sent.take().unwrap_or_default();
                context_sent.insert(actor.to_string(), true);
                state.active_run = None;
                state.round = round;
                state.rounds_in_window = Some(rounds_in_window);
                state.next_actor = next_actor.clone();
                state.context_hash = Some(context_hash.clone());
                state.context_sent = Some(context_sent);
                state.latest_failure = None;
                state.last_error = None;
                state.consecutive_failures = Some(0);
                state.compact_retries = Some(0);
                state.updated_at = Some(now.clone());
                match actor {
                    "claude" => {
                        if let Some(sid) = &session_id {
                            state.claude_session_id = Some(sid.clone());
                        }
                    }
                    "codex" => {
                        if let Some(tid) = &thread_id {
                            state.codex_thread_id = Some(tid.clone());
                        }
                    }
                    "cursor" => {
                        if let Some(sid) = &session_id {
                            state.cursor_session_id = Some(sid.clone());
                        }
                    }
                    "opencode" => {
                        if let Some(sid) = &session_id {
                            state.opencode_session_id = Some(sid.clone());
                        }
                    }
                    "kimi" => {
                        if let Some(sid) = &session_id {
                            state.kimi_session_id = Some(sid.clone());
                        }
                    }
                    _ => {}
                }

                if break_confirmed {
                    state.status = if has_queued_instructions {
                        TaskStatus::Ready
                    } else {
                        TaskStatus::Done
                    };
                    state.countdown = None;
                    state.pending_break = None;
                    state.break_rejected_by = None;
                    return state;
                }

                if break_pending {
                    state.status = if round_window_reached {
                        TaskStatus::Paused
                    } else {
                        TaskStatus::Ready
                    };
                    state.pending_break = Some(BreakMarker {
                        actor: Some(actor.to_string()),
                        round: Some(round),
                    });
                    state.break_rejected_by = None;
                    state.countdown = None;
                    return state;
                }

                state.status = if round_window_reached {
                    TaskStatus::Paused
                } else {
                    TaskStatus::Ready
                };
                state.pending_break = if break_rejected {
                    None
                } else {
                    pending_break_for_update.clone()
                };
                state.break_rejected_by = if break_rejected {
                    Some(BreakMarker {
                        actor: Some(actor.to_string()),
                        round: Some(round),
                    })
                } else {
                    None
                };
                state.countdown = None;
                state
            })
            .await?;

        if break_confirmed {
            self.store
                .append_task_event(
                    task_id,
                    workspace_key,
                    EventInput {
                        event_type: "actor.finished".to_string(),
                        actor: Some(actor.to_string()),
                        run_id: Some(run_id.to_string()),
                        payload: payload(serde_json::json!({
                            "elapsed_ms": elapsed_ms,
                            "exit_code": exit_code,
                            "buddy_type": "break_confirmed",
                        })),
                        ..Default::default()
                    },
                )
                .await?;

            let first_actor = pending_break
                .as_ref()
                .and_then(|p| p.actor.clone())
                .unwrap_or_default();
            if has_queued_instructions {
                let mut meta = Map::new();
                meta.insert("kind".to_string(), Value::from("round_notice"));
                meta.insert("round".to_string(), Value::from(round));
                self.store
                    .append_transcript(
                        task_id,
                        workspace_key,
                        "system",
                        &format!(
                            "{} 和 {} 均确认当前阶段完成，但指令队列中仍有待执行指令，继续执行。",
                            actor_display_name(&first_actor),
                            actor_display_name(actor)
                        ),
                        meta,
                    )
                    .await?;
                // Fall through to auto-start logic (skip duplicate actor.finished).
            } else {
                let task_stats = self.store.get_task_stats(task_id, workspace_key).await;
                let mut meta = Map::new();
                meta.insert("kind".to_string(), Value::from("round_notice"));
                meta.insert("round".to_string(), Value::from(round));
                meta.insert(
                    "done_reason".to_string(),
                    Value::from("dual_break_confirmed"),
                );
                if let Some(stats) = &task_stats {
                    meta.insert("stats".to_string(), serde_json::to_value(stats)?);
                }
                self.store
                    .append_transcript(
                        task_id,
                        workspace_key,
                        "system",
                        &format!(
                            "{} 和 {} 均确认任务完成，任务结束。",
                            actor_display_name(&first_actor),
                            actor_display_name(actor)
                        ),
                        meta,
                    )
                    .await?;
                self.store
                    .append_task_event(
                        task_id,
                        workspace_key,
                        EventInput {
                            event_type: "task.done".to_string(),
                            payload: payload(serde_json::json!({
                                "reason": "dual_break_confirmed",
                                "first_actor": first_actor,
                                "second_actor": actor,
                                "round": round,
                            })),
                            ..Default::default()
                        },
                    )
                    .await?;
                if let Some(notifier) = &self.notifier {
                    notifier
                        .notify_task_done(
                            task_id,
                            workspace_key,
                            "dual_break_confirmed",
                            Some(&first_actor),
                            Some(actor),
                        )
                        .await;
                }
                self.notify_terminal(workspace_key);
                return Ok(None);
            }
        }

        if break_pending {
            let mut meta = Map::new();
            meta.insert("kind".to_string(), Value::from("round_notice"));
            meta.insert("round".to_string(), Value::from(round));
            self.store
                .append_transcript(
                    task_id,
                    workspace_key,
                    "system",
                    &format!(
                        "{} 请求结束任务，等待 {} 确认。",
                        actor_display_name(actor),
                        actor_display_name(&next_actor)
                    ),
                    meta,
                )
                .await?;
            self.store
                .append_task_event(
                    task_id,
                    workspace_key,
                    EventInput {
                        event_type: "break.pending".to_string(),
                        actor: Some(actor.to_string()),
                        run_id: Some(run_id.to_string()),
                        payload: payload(serde_json::json!({
                            "elapsed_ms": elapsed_ms,
                            "exit_code": exit_code,
                            "buddy_type": "break",
                            "pending_confirmation_from": next_actor,
                        })),
                        ..Default::default()
                    },
                )
                .await?;
        } else if break_rejected {
            let rejected_from = pending_break
                .as_ref()
                .and_then(|p| p.actor.clone())
                .unwrap_or_default();
            self.store
                .append_task_event(
                    task_id,
                    workspace_key,
                    EventInput {
                        event_type: "break.rejected".to_string(),
                        actor: Some(actor.to_string()),
                        run_id: Some(run_id.to_string()),
                        payload: payload(serde_json::json!({
                            "rejected_break_from": rejected_from,
                        })),
                        ..Default::default()
                    },
                )
                .await?;
            let mut meta = Map::new();
            meta.insert("kind".to_string(), Value::from("round_notice"));
            meta.insert("round".to_string(), Value::from(round));
            self.store
                .append_transcript(
                    task_id,
                    workspace_key,
                    "system",
                    &format!(
                        "{} 认为任务尚未完成，{} 的结束请求已撤回。",
                        actor_display_name(actor),
                        actor_display_name(&rejected_from)
                    ),
                    meta,
                )
                .await?;
        }

        if !break_confirmed {
            self.store
                .append_task_event(
                    task_id,
                    workspace_key,
                    EventInput {
                        event_type: "actor.finished".to_string(),
                        actor: Some(actor.to_string()),
                        run_id: Some(run_id.to_string()),
                        payload: payload(serde_json::json!({
                            "elapsed_ms": elapsed_ms,
                            "exit_code": exit_code,
                            "buddy_type": buddy_type,
                        })),
                        ..Default::default()
                    },
                )
                .await?;
        }
        if round_window_reached {
            let mut meta = Map::new();
            meta.insert("kind".to_string(), Value::from("round_notice"));
            meta.insert("round".to_string(), Value::from(round));
            self.store
                .append_transcript(
                    task_id,
                    workspace_key,
                    "system",
                    &format!("{} 已达到轮次上限，暂停等待确认。", actor_display_name(actor)),
                    meta,
                )
                .await?;
            self.store
                .append_task_event(
                    task_id,
                    workspace_key,
                    EventInput {
                        event_type: "round_window.paused".to_string(),
                        payload: payload(serde_json::json!({
                            "max_rounds": max_rounds,
                            "rounds_in_window": rounds_in_window,
                            "next_actor": next_actor,
                        })),
                        ..Default::default()
                    },
                )
                .await?;
            self.notify_terminal(workspace_key);
            return Ok(None);
        }
        if self.execute_launchers {
            let advance: Result<StartTaskInput, RunnerError> = async {
                let queue_items = self
                    .store
                    .drain_instruction_queue(task_id, workspace_key)
                    .await?;
                if !queue_items.is_empty() {
                    self.send_queued_instructions(task_id, workspace_key, queue_items, &next_actor)
                        .await
                } else {
                    Ok(StartTaskInput {
                        workspace_key: Some(workspace_key.to_string()),
                        actor: Some(next_actor.clone()),
                        message: None,
                    })
                }
            }
            .await;
            // Auto-start of next actor failed; task is already in READY state.
            if let Ok(advance) = advance {
                return Ok(Some(advance));
            }
        }
        // NOTE: no notify_terminal here. A normal round that auto-advances to
        // the next actor has not reached a queue-relevant terminal state.
        Ok(None)
    }

    /// `markFailed`: record a run failure; auto-confirms a pending break from
    /// the other actor (→ DONE), and pauses when the consecutive-failure
    /// threshold is reached.
    async fn mark_failed(
        &self,
        task_id: &str,
        workspace_key: &str,
        actor: &str,
        message: &str,
        run_id: Option<&str>,
    ) -> Result<(), RunnerError> {
        // Guard: if the task was interrupted (active_run changed), skip.
        if let Some(run_id) = run_id {
            let current_state = self.store.read_task_state(task_id, workspace_key).await?;
            if current_state
                .active_run
                .as_ref()
                .and_then(|r| r.run_id.as_deref())
                != Some(run_id)
            {
                return Ok(());
            }
        }
        let failure = Failure {
            message: message.to_string(),
            actor: Some(actor.to_string()),
            run_id: None,
            ts: Some(utc_now()),
            output_file: None,
            event_file: None,
        };
        let state_before = self.store.read_task_state(task_id, workspace_key).await?;
        let other_actor_break = state_before.pending_break.clone().filter(|p| {
            p.actor.as_deref().map(|a| a != actor).unwrap_or(false)
        });

        if let Some(other_break) = other_actor_break {
            let round = state_before.round;
            let failure_ts = failure.ts.clone().unwrap_or_else(utc_now);
            self.store
                .update_task_state(task_id, workspace_key, |mut state| {
                    state.status = TaskStatus::Done;
                    state.active_run = None;
                    state.pending_break = None;
                    state.updated_at = Some(failure_ts.clone());
                    state
                })
                .await?;
            self.append_actor_failed_event(task_id, workspace_key, actor, run_id, message)
                .await?;
            let other_actor = other_break.actor.clone().unwrap_or_default();
            let mut meta = Map::new();
            meta.insert("kind".to_string(), Value::from("round_notice"));
            meta.insert("round".to_string(), Value::from(round));
            self.store
                .append_transcript(
                    task_id,
                    workspace_key,
                    "system",
                    &format!(
                        "{} 请求结束任务，{} 因错误无法继续，自动确认结束。",
                        actor_display_name(&other_actor),
                        actor_display_name(actor)
                    ),
                    meta,
                )
                .await?;
            self.store
                .append_task_event(
                    task_id,
                    workspace_key,
                    EventInput {
                        event_type: "task.done".to_string(),
                        payload: payload(serde_json::json!({
                            "reason": "break_confirmed_on_failure",
                            "first_actor": other_actor,
                            "second_actor": actor,
                            "round": round,
                        })),
                        ..Default::default()
                    },
                )
                .await?;
            if let Some(notifier) = &self.notifier {
                notifier
                    .notify_task_done(
                        task_id,
                        workspace_key,
                        "break_confirmed_on_failure",
                        Some(&other_actor),
                        Some(actor),
                    )
                    .await;
            }
            self.notify_terminal(workspace_key);
            return Ok(());
        }

        let new_consecutive_failures = state_before.consecutive_failures.unwrap_or(0) + 1;
        let global_settings = self.store.read_global_settings().await?;
        let max_consecutive_failures = global_settings.max_consecutive_failures.unwrap_or(10);
        let threshold_reached = new_consecutive_failures >= max_consecutive_failures;

        let failure_for_state = failure.clone();
        self.store
            .update_task_state(task_id, workspace_key, |mut state| {
                state.status = if threshold_reached {
                    TaskStatus::Paused
                } else {
                    TaskStatus::Failed
                };
                state.active_run = None;
                state.consecutive_failures = Some(new_consecutive_failures);
                state.last_error = Some(failure_for_state.clone());
                state.latest_failure = Some(failure_for_state.clone());
                state.compact_retries = Some(0);
                state.updated_at = failure_for_state.ts.clone();
                state
            })
            .await?;
        self.append_actor_failed_event(task_id, workspace_key, actor, run_id, message)
            .await?;
        if threshold_reached {
            let mut meta = Map::new();
            meta.insert("kind".to_string(), Value::from("round_notice"));
            meta.insert("round".to_string(), Value::from(state_before.round));
            self.store
                .append_transcript(
                    task_id,
                    workspace_key,
                    "system",
                    &format!(
                        "{} 连续失败 {new_consecutive_failures} 次，已达到上限 ({max_consecutive_failures})，暂停等待用户处理。",
                        actor_display_name(actor)
                    ),
                    meta,
                )
                .await?;
            self.store
                .append_task_event(
                    task_id,
                    workspace_key,
                    EventInput {
                        event_type: "failure_threshold.reached".to_string(),
                        payload: payload(serde_json::json!({
                            "consecutive_failures": new_consecutive_failures,
                            "max_consecutive_failures": max_consecutive_failures,
                        })),
                        ..Default::default()
                    },
                )
                .await?;
            if let Some(notifier) = &self.notifier {
                notifier
                    .notify_task_paused(
                        task_id,
                        workspace_key,
                        actor,
                        new_consecutive_failures,
                        max_consecutive_failures,
                    )
                    .await;
            }
        } else if let Some(notifier) = &self.notifier {
            notifier
                .notify_task_failed(task_id, workspace_key, actor, message)
                .await;
        }
        self.notify_terminal(workspace_key);
        Ok(())
    }

    async fn append_actor_failed_event(
        &self,
        task_id: &str,
        workspace_key: &str,
        actor: &str,
        run_id: Option<&str>,
        message: &str,
    ) -> Result<(), RunnerError> {
        // TS payload: { error, run_id } with `run_id` dropped when undefined.
        let mut event_payload = Map::new();
        event_payload.insert("error".to_string(), Value::from(message));
        if let Some(run_id) = run_id {
            event_payload.insert("run_id".to_string(), Value::from(run_id));
        }
        self.store
            .append_task_event(
                task_id,
                workspace_key,
                EventInput {
                    event_type: "actor.failed".to_string(),
                    actor: Some(actor.to_string()),
                    run_id: run_id.map(str::to_string),
                    payload: event_payload,
                    ..Default::default()
                },
            )
            .await?;
        Ok(())
    }

    /// Reset the session for an actor: clear the session ID, mark context as
    /// unsent, and write a compact context summary (LLM-generated when
    /// possible, truncated fallback otherwise) to `context.md`.
    async fn reset_session_for_actor(
        &self,
        task_id: &str,
        workspace_key: &str,
        actor: &str,
        detail: &TaskDetail,
    ) -> Result<(), RunnerError> {
        let session_key = match session_field_for_actor(actor) {
            Some(key) => key,
            None => return Ok(()),
        };

        let task_directory = self.store.task_directory(task_id, workspace_key);
        let cwd = existing_cwd(detail.state.repo_root.as_deref()).await;
        let launcher = detail
            .settings
            .launchers
            .get(actor)
            .cloned()
            .unwrap_or(Launcher {
                command: actor.to_string(),
                env: HashMap::new(),
                timeout_seconds: 600,
            });
        let summary_context = self
            .summarize_context_via_llm(task_id, workspace_key, actor, detail, &cwd, &launcher)
            .await?;

        let compact_context = summary_context.clone().unwrap_or_else(|| {
            build_compact_context_fallback(
                &detail.task_text,
                &detail.context_text,
                &detail.transcript,
                actor,
            )
        });

        self.store
            .update_task_state(task_id, workspace_key, |mut state| {
                let mut context_sent = state.context_sent.take().unwrap_or_default();
                context_sent.insert(actor.to_string(), false);
                match session_key {
                    "claude_session_id" => state.claude_session_id = None,
                    "codex_thread_id" => state.codex_thread_id = None,
                    "cursor_session_id" => state.cursor_session_id = None,
                    "opencode_session_id" => state.opencode_session_id = None,
                    "kimi_session_id" => state.kimi_session_id = None,
                    _ => {}
                }
                state.context_sent = Some(context_sent);
                state
            })
            .await?;

        // Write the compact context to context.md so it gets picked up by
        // buildActorPrompt on the next executeActor call. Back up the original
        // context.md first so the full context is not lost.
        let context_file = task_directory.join("context.md");
        let backup_file = task_directory.join("context.full.md");
        if let Ok(original) = tokio::fs::read_to_string(&context_file).await {
            if !original.trim().is_empty() {
                tokio::fs::write(&backup_file, &original).await?;
            }
        }
        tokio::fs::write(&context_file, &compact_context).await?;

        self.store
            .append_task_event(
                task_id,
                workspace_key,
                EventInput {
                    event_type: "actor.session_reset".to_string(),
                    actor: Some(actor.to_string()),
                    run_id: Some(new_run_id("reset")),
                    payload: payload(serde_json::json!({
                        "reason": "context_window_limit",
                        "session_key": session_key,
                        "summary_method": if summary_context.is_some() { "llm" } else { "truncation" },
                    })),
                    ..Default::default()
                },
            )
            .await?;
        Ok(())
    }

    /// Use an LLM to summarize the task context and transcript into a compact
    /// summary for the fresh session. Returns `None` on failure (caller falls
    /// back to simple truncation).
    async fn summarize_context_via_llm(
        &self,
        task_id: &str,
        workspace_key: &str,
        actor: &str,
        detail: &TaskDetail,
        cwd: &str,
        launcher: &Launcher,
    ) -> Result<Option<String>, RunnerError> {
        let summarize_prompt = build_summarize_prompt(
            &detail.task_text,
            &detail.context_text,
            &detail.transcript,
        );

        // Pre-check: skip LLM summarization when the prompt is still very
        // large after size limiting. Rough estimate: 1 token ≈ 4 chars for
        // English, ≈ 2 chars for CJK.
        let estimated_tokens = summarize_prompt.chars().count().div_ceil(3);
        let max_tokens_for_summarize = 100_000;
        if estimated_tokens > max_tokens_for_summarize {
            self.store
                .append_task_event(
                    task_id,
                    workspace_key,
                    EventInput {
                        event_type: "actor.summarize_skipped".to_string(),
                        actor: Some(actor.to_string()),
                        run_id: Some(new_run_id("summarize")),
                        payload: payload(serde_json::json!({
                            "reason": "prompt_too_large",
                            "estimated_tokens": estimated_tokens,
                            "char_count": summarize_prompt.chars().count(),
                        })),
                        ..Default::default()
                    },
                )
                .await?;
            return Ok(None);
        }

        let task_directory = self.store.task_directory(task_id, workspace_key);
        let artifacts_dir = task_directory.join("artifacts");
        tokio::fs::create_dir_all(&artifacts_dir).await?;
        let summarize_run_id = new_run_id("summarize");
        let summarize_prompt_file = artifacts_dir.join(format!("{summarize_run_id}-prompt.md"));
        tokio::fs::write(&summarize_prompt_file, &summarize_prompt).await?;

        let summarize_command = build_launcher_command(&LauncherCommandInput {
            actor: actor.to_string(),
            command: launcher.command.clone(),
            mode: Some("start".to_string()), // fresh session, no resume
            prompt_text: Some(summarize_prompt),
            prompt_file: summarize_prompt_file.to_string_lossy().to_string(),
            repo_root: Some(cwd.to_string()),
            task_dir: Some(task_directory.to_string_lossy().to_string()),
            run_id: Some(summarize_run_id.clone()),
            ..Default::default()
        });

        let output_lines = Arc::new(Mutex::new(Vec::<String>::new()));
        let stderr_lines = Arc::new(Mutex::new(Vec::<String>::new()));
        let mut merged_env = launcher.env.clone();
        if let Some(command_env) = &summarize_command.env {
            merged_env.extend(command_env.clone());
        }

        let result: Result<LauncherRunResult, RunnerError> = if kind_needs_pty(summarize_command.kind) {
            if let Some(shim_dir) = ensure_osascript_shim_dir(&self.store.data_root).await {
                prepend_path(&mut merged_env, &shim_dir);
            }
            let output_lines = output_lines.clone();
            run_launcher_with_pty(
                &PtyRunInput {
                    command: summarize_command.command.clone(),
                    args: summarize_command.args.clone(),
                    cwd: cwd.to_string(),
                    env: Some(merged_env),
                    timeout_ms: 120_000,
                    abort: None,
                },
                move |data| {
                    for line in data
                        .split('\n')
                        .map(|l| l.strip_suffix('\r').unwrap_or(l))
                        .filter(|l| !l.is_empty())
                    {
                        output_lines.lock().push(line.to_string());
                    }
                },
            )
            .await
            .map_err(RunnerError::from)
        } else {
            let output_lines = output_lines.clone();
            let stderr_lines = stderr_lines.clone();
            run_launcher(
                &RunLauncherInput {
                    command: summarize_command.command.clone(),
                    args: summarize_command.args.clone(),
                    cwd: cwd.to_string(),
                    env: Some(merged_env),
                    stdin_text: summarize_command.stdin_text.clone(),
                    timeout_ms: 120_000, // 2 minutes for summarization
                    abort: None,
                },
                move |line| output_lines.lock().push(line),
                move |line| stderr_lines.lock().push(line),
            )
            .await
            .map_err(RunnerError::from)
        };

        match result {
            Ok(result) if result.exit_code == Some(0) => {
                let stdout_text = output_lines.lock().join("\n");
                let extracted = extract_actor_output(
                    &parser_actor_for_kind(actor, summarize_command.kind),
                    &stdout_text,
                );
                if extracted.trim().is_empty() {
                    self.store
                        .append_task_event(
                            task_id,
                            workspace_key,
                            EventInput {
                                event_type: "actor.summarize_failed".to_string(),
                                actor: Some(actor.to_string()),
                                run_id: Some(new_run_id("summarize")),
                                payload: payload(serde_json::json!({ "reason": "empty_output" })),
                                ..Default::default()
                            },
                        )
                        .await?;
                    return Ok(None);
                }
                self.store
                    .append_task_event(
                        task_id,
                        workspace_key,
                        EventInput {
                            event_type: "actor.summarize_succeeded".to_string(),
                            actor: Some(actor.to_string()),
                            run_id: Some(new_run_id("summarize")),
                            payload: payload(serde_json::json!({
                                "summary_length": extracted.chars().count(),
                            })),
                            ..Default::default()
                        },
                    )
                    .await?;
                Ok(Some(extracted.trim().to_string()))
            }
            Ok(result) => {
                let stderr_text = stderr_lines.lock().join("\n").trim().to_string();
                self.store
                    .append_task_event(
                        task_id,
                        workspace_key,
                        EventInput {
                            event_type: "actor.summarize_failed".to_string(),
                            actor: Some(actor.to_string()),
                            run_id: Some(new_run_id("summarize")),
                            payload: payload(serde_json::json!({
                                "exit_code": result.exit_code,
                                "stderr_preview": truncate_chars(&stderr_text, 1000),
                            })),
                            ..Default::default()
                        },
                    )
                    .await?;
                Ok(None)
            }
            Err(error) => {
                self.store
                    .append_task_event(
                        task_id,
                        workspace_key,
                        EventInput {
                            event_type: "actor.summarize_failed".to_string(),
                            actor: Some(actor.to_string()),
                            run_id: Some(new_run_id("summarize")),
                            payload: payload(serde_json::json!({
                                "error": truncate_chars(&error.to_string(), 1000),
                            })),
                            ..Default::default()
                        },
                    )
                    .await?;
                Ok(None)
            }
        }
    }

    /// `drainAllInstructions` / `sendQueuedInstructions`: between rounds,
    /// queued instructions are flushed as human messages; returns the
    /// follow-up start for the next actor (executed by the `start_task`
    /// trampoline).
    async fn send_queued_instructions(
        &self,
        task_id: &str,
        workspace_key: &str,
        items: Vec<InstructionQueueItem>,
        next_actor: &str,
    ) -> Result<StartTaskInput, RunnerError> {
        let combined_content = items
            .iter()
            .map(|item| item.content.clone())
            .collect::<Vec<_>>()
            .join("\n\n");
        for item in &items {
            let mut meta = Map::new();
            meta.insert("source".to_string(), Value::from("instruction_queue"));
            meta.insert("queue_item_id".to_string(), Value::from(item.id.clone()));
            if let Some(attachments) = &item.attachments {
                if !attachments.is_empty() {
                    meta.insert("attachments".to_string(), serde_json::to_value(attachments)?);
                }
            }
            self.store
                .append_transcript(task_id, workspace_key, "human", &item.content, meta)
                .await?;
            self.store
                .append_task_event(
                    task_id,
                    workspace_key,
                    EventInput {
                        event_type: "human.message".to_string(),
                        payload: payload(serde_json::json!({
                            "content": item.content,
                            "source": "instruction_queue",
                        })),
                        ..Default::default()
                    },
                )
                .await?;
        }
        Ok(StartTaskInput {
            workspace_key: Some(workspace_key.to_string()),
            actor: Some(next_actor.to_string()),
            message: Some(combined_content),
        })
    }
}

#[async_trait]
impl QueueTaskRunner for BuddyRunner {
    async fn start_task(&self, task_id: &str, workspace_key: &str) -> Result<(), String> {
        BuddyRunner::start_task(
            self,
            task_id,
            StartTaskInput {
                workspace_key: Some(workspace_key.to_string()),
                ..Default::default()
            },
        )
        .await
        .map(|_| ())
        .map_err(|error| error.to_string())
    }
}

/// Outcome of a single connectivity ping (`executePingAttempt`).
#[derive(Debug, Default)]
struct PingOutcome {
    success: bool,
    session_id: Option<String>,
    thread_id: Option<String>,
    error: Option<String>,
    stderr: String,
    stdout: String,
}

/// A failed actor run attempt, carrying everything the retry/marking logic
/// needs (TS: the catch block's closure over `stderrLines`/`outputLines`).
struct ActorRunFailure {
    message: String,
    stderr_text: String,
    stdout_text: String,
    detail: TaskDetail,
    global_settings: GlobalSettings,
}

enum AttemptError {
    /// Setup errors that in TS escape the try/catch (no markFailed).
    Fatal(RunnerError),
    /// Run errors that in TS are caught and drive retry / markFailed.
    Run(ActorRunFailure),
}

fn can_start_from(status: &TaskStatus) -> bool {
    matches!(
        status,
        TaskStatus::Ready
            | TaskStatus::Paused
            | TaskStatus::Failed
            | TaskStatus::Countdown
            | TaskStatus::Done
            | TaskStatus::Pinging
    )
}

/// `needsHealthCheck`: a fresh task (round 0, no sessions, no clean prior
/// health check) must ping both actors before the first run.
pub fn needs_health_check(state: &TaskState, settings: &TaskSettings) -> bool {
    if state.round > 0 {
        return false;
    }
    // A clean prior pass has no failed_actor; only then do we skip the ping.
    if let Some(health_check) = &state.health_check {
        if health_check
            .failed_actor
            .as_deref()
            .map(str::is_empty)
            .unwrap_or(true)
        {
            return false;
        }
    }
    let implementer = settings.implementer_actor.clone().unwrap_or_else(|| {
        if settings.role_mode == "codex_implements" {
            "codex".to_string()
        } else {
            "claude".to_string()
        }
    });
    let reviewer = settings.reviewer_actor.clone().unwrap_or_else(|| {
        if settings.role_mode == "codex_implements" {
            "claude".to_string()
        } else {
            "codex".to_string()
        }
    });
    let impl_session = session_id_for_actor(&implementer, state, Some(settings));
    let rev_session = session_id_for_actor(&reviewer, state, Some(settings));
    impl_session.is_none() && rev_session.is_none()
}

/// `sessionIdForActor`: current session id, falling back to the seed from
/// task settings (empty strings count as unset).
pub fn session_id_for_actor(
    actor: &str,
    state: &TaskState,
    settings: Option<&TaskSettings>,
) -> Option<String> {
    let seed = match actor {
        "claude" => settings.and_then(|s| s.seed_claude_session_id.clone()),
        "codex" => settings.and_then(|s| s.seed_codex_thread_id.clone()),
        "cursor" => settings.and_then(|s| s.seed_cursor_session_id.clone()),
        "opencode" => settings.and_then(|s| s.seed_opencode_session_id.clone()),
        "kimi" => settings.and_then(|s| s.seed_kimi_session_id.clone()),
        _ => None,
    }
    .filter(|value| !value.is_empty());
    let current = match actor {
        "claude" => state.claude_session_id.clone(),
        "codex" => state.codex_thread_id.clone(),
        "cursor" => state.cursor_session_id.clone(),
        "opencode" => state.opencode_session_id.clone(),
        "kimi" => state.kimi_session_id.clone(),
        _ => None,
    };
    current.or(seed)
}

/// State field name holding a given actor's session id.
fn session_field_for_actor(actor: &str) -> Option<&'static str> {
    match actor {
        "claude" => Some("claude_session_id"),
        "codex" => Some("codex_thread_id"),
        "cursor" => Some("cursor_session_id"),
        "opencode" => Some("opencode_session_id"),
        "kimi" => Some("kimi_session_id"),
        _ => None,
    }
}

fn set_session_field(state: &mut TaskState, key: &str, value: String) {
    match key {
        "claude_session_id" => state.claude_session_id = Some(value),
        "codex_thread_id" => state.codex_thread_id = Some(value),
        "cursor_session_id" => state.cursor_session_id = Some(value),
        "opencode_session_id" => state.opencode_session_id = Some(value),
        "kimi_session_id" => state.kimi_session_id = Some(value),
        _ => {}
    }
}

fn normalize_actor_role(actor: &str) -> &'static str {
    match actor {
        "claude" => "claude",
        "codex" => "codex",
        "cursor" => "cursor",
        "opencode" => "opencode",
        "kimi" => "kimi",
        _ => "system",
    }
}

/// `existingCwd`: the repo root when it exists, otherwise the process cwd.
async fn existing_cwd(path: Option<&str>) -> String {
    if let Some(path) = path.filter(|p| !p.is_empty()) {
        if tokio::fs::metadata(path).await.is_ok() {
            return path.to_string();
        }
    }
    std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| ".".to_string())
}

/// `collectRawEvents`: raw JSONL event stream for the parser, from the event
/// file (contract launchers write it themselves) and/or captured stdout.
pub async fn collect_raw_events(
    event_file: &Path,
    stdout_text: &str,
    kind: LauncherCommandKind,
) -> String {
    if kind != LauncherCommandKind::Contract {
        if !stdout_text.is_empty() {
            let _ = tokio::fs::write(event_file, stdout_text).await;
        }
        return stdout_text.to_string();
    }

    let file_text = read_optional_text(event_file).await;
    if !file_text.is_empty() && !stdout_text.is_empty() {
        return format!("{}\n{}", file_text.trim_end(), stdout_text);
    }
    if !file_text.is_empty() {
        return file_text;
    }
    if !stdout_text.is_empty() {
        let _ = tokio::fs::write(event_file, stdout_text).await;
        return stdout_text.to_string();
    }
    String::new()
}

/// `collectOutputText`: the actor's final text output. Native kinds extract
/// from the event stream (with the opencode/kimi tool_use break fallback) and
/// persist a normalized buddy JSON to the output file; other kinds prefer the
/// output file the launcher wrote.
pub async fn collect_output_text(
    actor: &str,
    kind: LauncherCommandKind,
    output_file: &Path,
    stdout_text: &str,
) -> String {
    if matches!(
        kind,
        LauncherCommandKind::NativeClaude
            | LauncherCommandKind::NativeCursor
            | LauncherCommandKind::NativeOpencode
            | LauncherCommandKind::NativeKimi
    ) {
        let parser_actor = parser_actor_for_kind(actor, kind);
        let mut output = extract_actor_output(&parser_actor, stdout_text);
        let mut message = parse_buddy_message(&output);

        // Fallback: some models (e.g. DeepSeek via OpenCode/Kimi) output buddy
        // JSON via echo/bash commands; the message appears in part.state.output
        // of tool_use events.
        if !matches!(message, BuddyMessage::Break { .. })
            && matches!(
                kind,
                LauncherCommandKind::NativeOpencode | LauncherCommandKind::NativeKimi
            )
        {
            for event in parse_jsonl_buffer(stdout_text) {
                if event.get("type").and_then(Value::as_str) == Some("tool_use") {
                    if let Some(part) = event.get("part").filter(|p| p.is_object()) {
                        if let Some(state) = part.get("state").filter(|s| s.is_object()) {
                            if let Some(tool_output) =
                                state.get("output").and_then(Value::as_str)
                            {
                                let tool_message = parse_buddy_message(tool_output.trim());
                                if matches!(tool_message, BuddyMessage::Break { .. }) {
                                    message = tool_message;
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        }

        // If break was found in tool output but not in extracted text, prepend
        // it so completeActor's parseBuddyMessage call detects it.
        if let BuddyMessage::Break { content, .. } = &message {
            if !matches!(parse_buddy_message(&output), BuddyMessage::Break { .. }) {
                let break_json = serde_json::to_string(&serde_json::json!({
                    "type": "break",
                    "content": content,
                }))
                .unwrap_or_default();
                output = format!("{break_json}\n{output}");
            }
        }

        let normalized = match &message {
            BuddyMessage::Break { content, .. } => serde_json::json!({
                "type": "break",
                "content": content,
            }),
            BuddyMessage::Message { text } => serde_json::json!({
                "type": "chat",
                "content": text,
            }),
        };
        let _ = tokio::fs::write(output_file, normalized.to_string()).await;
        return output;
    }

    if tokio::fs::metadata(output_file).await.is_ok() {
        return read_optional_text(output_file).await;
    }
    let extracted = extract_actor_output(&parser_actor_for_kind(actor, kind), stdout_text);
    if !extracted.is_empty() {
        extracted
    } else {
        stdout_text.to_string()
    }
}

async fn read_optional_text(path: &Path) -> String {
    tokio::fs::read_to_string(path).await.unwrap_or_default()
}

/// `buildSummarizePrompt`: condense task context + transcript into a compact
/// summary prompt for the fresh session, with hard size limits.
fn build_summarize_prompt(
    task_text: &str,
    context_text: &str,
    transcript: &[TranscriptEntry],
) -> String {
    let mut parts: Vec<String> = Vec::new();

    parts.push("请将以下任务上下文和对话记录总结为一份精简摘要。".to_string());
    parts.push(String::new());
    parts.push("要求：".to_string());
    parts.push("1. 保留关键决策、发现和未解决问题".to_string());
    parts.push("2. 去除冗余和重复信息".to_string());
    parts.push("3. 保持摘要简洁（不超过 2000 字）".to_string());
    parts.push("4. 用 markdown 格式输出".to_string());
    parts.push("5. 直接输出摘要内容，不要有额外的解释或前言".to_string());
    parts.push(String::new());

    if !task_text.trim().is_empty() {
        parts.push("## 任务描述".to_string());
        parts.push(task_text.trim().to_string());
        parts.push(String::new());
    }

    if !context_text.trim().is_empty() {
        // Truncate context to avoid prompt being too large.
        let ctx = context_text.trim();
        let max_ctx_len = 10_000;
        if ctx.chars().count() > max_ctx_len {
            parts.push("## 背景上下文（已截断）".to_string());
            parts.push(truncate_chars(ctx, max_ctx_len));
            parts.push("...（上下文已截断）".to_string());
        } else {
            parts.push("## 背景上下文".to_string());
            parts.push(ctx.to_string());
        }
        parts.push(String::new());
    }

    // Include only the most recent transcript entries, with size limits.
    if !transcript.is_empty() {
        parts.push("## 对话记录".to_string());
        let start = transcript.len().saturating_sub(SUMMARIZE_MAX_TRANSCRIPT_ENTRIES);
        for entry in &transcript[start..] {
            let role_label = match entry.role.as_str() {
                "human" => "用户".to_string(),
                "system" => "系统".to_string(),
                other => actor_display_name(other),
            };
            let content = if entry.content.chars().count() > SUMMARIZE_MAX_ENTRY_CHARS {
                format!(
                    "{}...（已截断）",
                    truncate_chars(&entry.content, SUMMARIZE_MAX_ENTRY_CHARS)
                )
            } else {
                entry.content.clone()
            };
            parts.push(format!("### {role_label}"));
            parts.push(content);
            parts.push(String::new());
        }
    }

    parts.push("## 输出要求".to_string());
    parts.push("请输出一份精简的上下文摘要，包含：当前进展、关键发现、待解决问题。不要输出任何其他内容。".to_string());

    let mut result = parts.join("\n");

    // Hard limit: if the prompt exceeds the max size, truncate.
    if result.chars().count() > SUMMARIZE_MAX_PROMPT_CHARS {
        result = format!(
            "{}\n\n...（提示词已截断，请基于以上内容总结）",
            truncate_chars(&result, SUMMARIZE_MAX_PROMPT_CHARS)
        );
    }

    result
}

/// `buildCompactContextFallback`: truncation-based compact context for a fresh
/// session when LLM summarization fails.
fn build_compact_context_fallback(
    task_text: &str,
    context_text: &str,
    transcript: &[TranscriptEntry],
    _actor: &str,
) -> String {
    let mut parts: Vec<String> = Vec::new();

    parts.push("> ⚠️ 上一轮会话因上下文窗口限制已重置。以下是精简后的上下文摘要。".to_string());
    parts.push(String::new());

    if !task_text.trim().is_empty() {
        parts.push("## 任务".to_string());
        parts.push(task_text.trim().to_string());
        parts.push(String::new());
    }

    // Condense context text: take first 2000 chars if too long.
    if !context_text.trim().is_empty() {
        let trimmed = context_text.trim();
        if trimmed.chars().count() > 2000 {
            parts.push("## 背景上下文（已截断）".to_string());
            parts.push(truncate_chars(trimmed, 2000));
            parts.push("...（上下文已截断，详细内容请参考代码库）".to_string());
        } else {
            parts.push("## 背景上下文".to_string());
            parts.push(trimmed.to_string());
        }
        parts.push(String::new());
    }

    // Include only the last 2 transcript entries (most recent actor turns).
    let start = transcript.len().saturating_sub(2);
    let recent_transcript = &transcript[start..];
    if !recent_transcript.is_empty() {
        parts.push("## 最近对话（摘要）".to_string());
        for entry in recent_transcript {
            let role_label = match entry.role.as_str() {
                "human" => "用户".to_string(),
                "system" => "系统".to_string(),
                other => actor_display_name(other),
            };
            let content = if entry.content.chars().count() > 500 {
                format!("{}...（已截断）", truncate_chars(&entry.content, 500))
            } else {
                entry.content.clone()
            };
            parts.push(format!("**{role_label}**: {content}"));
        }
        parts.push(String::new());
    }

    parts.push("请基于以上摘要继续工作。如需更多上下文，请查阅代码库。".to_string());

    parts.join("\n")
}

/// `{prefix}_{millis}_{6 hex chars}`, mirroring TS
/// `` `${prefix}_${Date.now()}_${Math.random().toString(16).slice(2, 8)}` ``.
fn new_run_id(prefix: &str) -> String {
    let millis = Utc::now().timestamp_millis();
    let hex = uuid::Uuid::new_v4().simple().to_string();
    format!("{prefix}_{millis}_{}", &hex[..6])
}

/// Timestamp matching the store's `utc_now` (`%Y-%m-%dT%H:%M:%SZ`).
fn utc_now() -> String {
    Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Convert a `serde_json::json!` object into the owned payload map the store
/// expects. Non-object values yield an empty map (never happens at call sites).
fn payload(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}

/// JS `string.slice(0, n)` on a char boundary (Rust-safe equivalent).
fn truncate_chars(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}


// ---------------------------------------------------------------------------
// Tests — port of the runner-related suites in `tests/unit/main/` of the
// Electron edition. Node.js fake launchers from the TS tests are replaced by
// equivalent POSIX shell scripts (`sh` is always a "contract" launcher).
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use crate::buddy::types::CreateTaskInput;
    use serde_json::json;
    use std::path::Path;
    use tempfile::TempDir;

    fn live_runner(store: &Arc<BuddyStore>) -> BuddyRunner {
        BuddyRunner::new(store.clone(), RunnerOptions::default())
    }

    fn deferred_runner(store: &Arc<BuddyStore>) -> BuddyRunner {
        BuddyRunner::new(
            store.clone(),
            RunnerOptions {
                execute_launchers: Some(false),
                ..Default::default()
            },
        )
    }

    async fn write_fake(dir: &Path, name: &str, body: &str) -> String {
        let path = dir.join(name);
        tokio::fs::write(&path, body).await.unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).unwrap();
        }
        path.to_string_lossy().to_string()
    }

    async fn create_demo_task(
        store: &BuddyStore,
        root: &TempDir,
        settings: Option<HashMap<String, Value>>,
    ) -> crate::buddy::types::CreateTaskResult {
        store
            .create_task(CreateTaskInput {
                task_id: "demo".to_string(),
                repo_root: Some(root.path().to_string_lossy().to_string()),
                task_text: None,
                context_text: None,
                settings,
                execution_mode: None,
            })
            .await
            .unwrap()
    }

    fn settings_map(entries: &[(&str, Value)]) -> Option<HashMap<String, Value>> {
        Some(
            entries
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
        )
    }

    async fn patch_global_settings(store: &BuddyStore, patch: Value) {
        let current =
            serde_json::to_value(store.read_global_settings().await.unwrap()).unwrap();
        let mut obj = current.as_object().unwrap().clone();
        for (key, value) in patch.as_object().unwrap() {
            obj.insert(key.clone(), value.clone());
        }
        let settings: GlobalSettings = serde_json::from_value(Value::Object(obj)).unwrap();
        store.update_global_settings(&settings).await.unwrap();
    }

    fn event_types(detail: &TaskDetail) -> Vec<&str> {
        detail
            .events
            .iter()
            .map(|event| event.event_type.as_str())
            .collect()
    }

    /// Contract fake that answers with a chat buddy message via the output file.
    const CHAT_ACTOR: &str = r#"#!/bin/sh
printf '{"type":"chat","content":"%s chat"}' "$BUDDY_ACTOR" > "$BUDDY_OUTPUT_FILE"
"#;

    /// Contract fake that always signals break via the output file.
    const BREAK_ACTOR: &str = r#"#!/bin/sh
printf '{"type":"break","content":"%s confirms done"}' "$BUDDY_ACTOR" > "$BUDDY_OUTPUT_FILE"
"#;

    /// Contract fake that always fails with an error on stderr.
    const FAILING_ACTOR: &str = "#!/bin/sh\nprintf 'boom\\n' >&2\nexit 2\n";

    /// Contract fake that fails with a context window limit error.
    const CONTEXT_LIMIT_ACTOR: &str = "#!/bin/sh\nprintf '%s\\n' 'API Error: The model has reached its context window limit.' >&2\nexit 1\n";

    // -----------------------------------------------------------------------
    // buddy-runner.test.ts — state transitions
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn ready_task_moves_to_running_actor_state_when_launchers_deferred() {
        let root = TempDir::new().unwrap();
        let store = Arc::new(BuddyStore::new(root.path()));
        let created = create_demo_task(&store, &root, None).await;
        let runner = deferred_runner(&store);

        let run_id = runner
            .start_task(
                "demo",
                StartTaskInput {
                    workspace_key: Some(created.workspace_key.clone()),
                    actor: Some("claude".to_string()),
                    message: None,
                },
            )
            .await
            .unwrap();

        let detail = store
            .get_task_detail("demo", &created.workspace_key)
            .await
            .unwrap();
        assert!(run_id.starts_with("run_"));
        assert_eq!(detail.state.status, TaskStatus::RunningClaude);
        assert_eq!(
            detail.state.active_run.as_ref().map(|r| r.actor.as_str()),
            Some("claude")
        );
    }

    #[tokio::test]
    async fn send_message_during_countdown_starts_selected_actor() {
        let root = TempDir::new().unwrap();
        let store = Arc::new(BuddyStore::new(root.path()));
        let created = create_demo_task(&store, &root, None).await;
        store
            .update_task_state("demo", &created.workspace_key, |mut state| {
                state.status = TaskStatus::Countdown;
                state.next_actor = "codex".to_string();
                state.countdown = Some(Countdown {
                    status: "running".to_string(),
                    remaining: Some(30),
                    started_at: None,
                    after_actor: None,
                    default_next_actor: "codex".to_string(),
                    deadline: None,
                });
                state
            })
            .await
            .unwrap();
        let runner = deferred_runner(&store);

        runner
            .send_message(
                "demo",
                SendMessageInput {
                    workspace_key: Some(created.workspace_key.clone()),
                    actor: Some("codex".to_string()),
                    message: Some("补充一下边界情况".to_string()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let detail = store
            .get_task_detail("demo", &created.workspace_key)
            .await
            .unwrap();
        assert_eq!(detail.state.status, TaskStatus::RunningCodex);
        assert_eq!(
            detail.state.active_run.as_ref().map(|r| r.actor.as_str()),
            Some("codex")
        );
        assert!(detail.state.countdown.is_none());
        assert_eq!(detail.transcript.len(), 1);
        assert_eq!(detail.transcript[0].role, "human");
        assert_eq!(detail.transcript[0].content, "补充一下边界情况");
        assert_eq!(
            detail.transcript[0]
                .meta
                .as_ref()
                .and_then(|m| m.get("source")),
            Some(&json!("run_once"))
        );
        let types = event_types(&detail);
        assert!(types.contains(&"human.message"));
        assert!(types.contains(&"actor.started"));
    }

    #[tokio::test]
    async fn pauses_before_starting_when_round_window_exhausted() {
        let root = TempDir::new().unwrap();
        let store = Arc::new(BuddyStore::new(root.path()));
        patch_global_settings(&store, json!({ "max_rounds": 1 })).await;
        let created = create_demo_task(&store, &root, None).await;
        store
            .update_task_state("demo", &created.workspace_key, |mut state| {
                state.rounds_in_window = Some(1);
                state
            })
            .await
            .unwrap();
        let runner = deferred_runner(&store);

        let error = runner
            .start_task(
                "demo",
                StartTaskInput {
                    workspace_key: Some(created.workspace_key.clone()),
                    actor: Some("claude".to_string()),
                    message: None,
                },
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("自动轮次上限"));

        let detail = store
            .get_task_detail("demo", &created.workspace_key)
            .await
            .unwrap();
        assert_eq!(detail.state.status, TaskStatus::Paused);
        let event = detail
            .events
            .iter()
            .find(|e| e.event_type == "round_window.paused")
            .expect("round_window.paused event");
        assert_eq!(event.payload.get("max_rounds"), Some(&json!(1)));
        assert_eq!(event.payload.get("rounds_in_window"), Some(&json!(1)));
    }

    #[tokio::test]
    async fn resuming_from_round_window_pause_resets_window() {
        let root = TempDir::new().unwrap();
        let store = Arc::new(BuddyStore::new(root.path()));
        patch_global_settings(&store, json!({ "max_rounds": 1 })).await;
        let created = create_demo_task(&store, &root, None).await;
        store
            .update_task_state("demo", &created.workspace_key, |mut state| {
                state.status = TaskStatus::Paused;
                state.rounds_in_window = Some(1);
                state
            })
            .await
            .unwrap();
        let runner = deferred_runner(&store);

        let run_id = runner
            .start_task(
                "demo",
                StartTaskInput {
                    workspace_key: Some(created.workspace_key.clone()),
                    actor: Some("claude".to_string()),
                    message: None,
                },
            )
            .await
            .unwrap();

        let detail = store
            .get_task_detail("demo", &created.workspace_key)
            .await
            .unwrap();
        assert!(run_id.starts_with("run_"));
        assert_eq!(detail.state.status, TaskStatus::RunningClaude);
        assert_eq!(detail.state.rounds_in_window, Some(0));
        let event = detail
            .events
            .iter()
            .find(|e| e.event_type == "round_window.reset")
            .expect("round_window.reset event");
        assert_eq!(event.payload.get("previous_rounds_in_window"), Some(&json!(1)));
        assert_eq!(event.payload.get("max_rounds"), Some(&json!(1)));
    }

    #[tokio::test]
    async fn does_not_notify_terminal_on_plain_deferred_start() {
        let root = TempDir::new().unwrap();
        let store = Arc::new(BuddyStore::new(root.path()));
        let created = create_demo_task(&store, &root, None).await;
        let runner = deferred_runner(&store);
        let calls = Arc::new(Mutex::new(Vec::<String>::new()));
        let calls_clone = calls.clone();
        runner.set_on_task_terminal(move |ws| calls_clone.lock().push(ws.to_string()));

        runner
            .start_task(
                "demo",
                StartTaskInput {
                    workspace_key: Some(created.workspace_key.clone()),
                    actor: Some("claude".to_string()),
                    message: None,
                },
            )
            .await
            .unwrap();
        assert!(calls.lock().is_empty());

        // Second half of the TS suite: a pending break alone does not fire the hook.
        store
            .update_task_state("demo", &created.workspace_key, |mut state| {
                state.pending_break = Some(BreakMarker {
                    actor: Some("codex".to_string()),
                    round: Some(1),
                });
                state
            })
            .await
            .unwrap();
        assert!(calls.lock().is_empty());
    }

    // -----------------------------------------------------------------------
    // buddy-runner-launcher.test.ts — fake launcher integration
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn records_actor_output_and_pauses_at_round_window() {
        let root = TempDir::new().unwrap();
        let fake = write_fake(
            root.path(),
            "fake-actor.sh",
            "#!/bin/sh\nprintf '%s\\n' '{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"done\"}],\"thread_id\":\"t1\"}'\n",
        )
        .await;
        let store = Arc::new(BuddyStore::new(root.path()));
        patch_global_settings(&store, json!({ "max_rounds": 1 })).await;
        let created = create_demo_task(
            &store,
            &root,
            settings_map(&[(
                "launchers",
                json!({ "codex": { "command": fake, "env": {}, "timeout_seconds": 5 } }),
            )]),
        )
        .await;
        let runner = live_runner(&store);

        runner
            .start_task(
                "demo",
                StartTaskInput {
                    workspace_key: Some(created.workspace_key.clone()),
                    actor: Some("codex".to_string()),
                    message: None,
                },
            )
            .await
            .unwrap();

        let detail = store
            .get_task_detail("demo", &created.workspace_key)
            .await
            .unwrap();
        assert_eq!(detail.state.status, TaskStatus::Paused);
        assert_eq!(detail.state.codex_thread_id.as_deref(), Some("t1"));
        assert!(event_types(&detail).contains(&"actor.completed"));

        let transcript_path = store.transcript_jsonl_path("demo", &created.workspace_key);
        let text = tokio::fs::read_to_string(transcript_path).await.unwrap();
        let first: Value =
            serde_json::from_str(text.lines().next().expect("one transcript row")).unwrap();
        assert_eq!(first["role"], json!("codex"));
        assert_eq!(first["content"], json!("done"));
        assert_eq!(first["meta"]["buddy_type"], json!("chat"));
        assert!(first["meta"]["elapsed_ms"].is_number());
    }

    #[tokio::test]
    async fn contract_launcher_receives_buddy_flags_and_env() {
        let root = TempDir::new().unwrap();
        let fake = write_fake(
            root.path(),
            "contract-actor.sh",
            r#"#!/bin/sh
args=" $* "
check() { case "$args" in *" $1 "*) ;; *) echo "missing $1" >&2; exit 1;; esac; }
check "--actor opencode"
check "--mode start"
check "--repo-root $BUDDY_REPO_ROOT"
check "--task-dir $BUDDY_TASK_DIR"
check "--run-id $BUDDY_RUN_ID"
check "--prompt-file $BUDDY_PROMPT_FILE"
check "--output-file $BUDDY_OUTPUT_FILE"
check "--event-file $BUDDY_EVENT_FILE"
[ "$BUDDY_ACTOR" = "opencode" ] || { echo 'missing actor env' >&2; exit 1; }
[ "$BUDDY_MODE" = "start" ] || { echo 'missing mode env' >&2; exit 1; }
printf '{"type":"chat","content":"custom output"}' > "$BUDDY_OUTPUT_FILE"
printf '%s\n' '{"type":"buddy.session","actor":"opencode","session_id":"custom-session"}' > "$BUDDY_EVENT_FILE"
"#,
        )
        .await;
        let store = Arc::new(BuddyStore::new(root.path()));
        patch_global_settings(&store, json!({ "max_rounds": 1 })).await;
        let created = create_demo_task(
            &store,
            &root,
            settings_map(&[(
                "launchers",
                json!({ "opencode": { "command": fake, "env": {}, "timeout_seconds": 5 } }),
            )]),
        )
        .await;
        let runner = live_runner(&store);

        runner
            .start_task(
                "demo",
                StartTaskInput {
                    workspace_key: Some(created.workspace_key.clone()),
                    actor: Some("opencode".to_string()),
                    message: None,
                },
            )
            .await
            .unwrap();

        let detail = store
            .get_task_detail("demo", &created.workspace_key)
            .await
            .unwrap();
        assert_eq!(
            detail.state.opencode_session_id.as_deref(),
            Some("custom-session")
        );
        assert!(detail
            .transcript
            .iter()
            .any(|t| t.role == "opencode" && t.content == "custom output"));
    }

    #[tokio::test]
    async fn hands_off_between_implementer_and_reviewer() {
        let root = TempDir::new().unwrap();
        let fake = write_fake(root.path(), "handoff-actor.sh", CHAT_ACTOR).await;
        let store = Arc::new(BuddyStore::new(root.path()));
        patch_global_settings(&store, json!({ "max_rounds": 2 })).await;
        let created = create_demo_task(
            &store,
            &root,
            settings_map(&[
                ("implementer_actor", json!("opencode")),
                ("reviewer_actor", json!("kimi")),
                (
                    "launchers",
                    json!({
                        "opencode": { "command": fake, "env": {}, "timeout_seconds": 5 },
                        "kimi": { "command": fake, "env": {}, "timeout_seconds": 5 }
                    }),
                ),
            ]),
        )
        .await;
        let runner = live_runner(&store);

        runner
            .start_task(
                "demo",
                StartTaskInput {
                    workspace_key: Some(created.workspace_key.clone()),
                    actor: Some("opencode".to_string()),
                    message: None,
                },
            )
            .await
            .unwrap();

        let detail = store
            .get_task_detail("demo", &created.workspace_key)
            .await
            .unwrap();
        assert_eq!(detail.state.status, TaskStatus::Paused);
        assert_eq!(detail.state.next_actor, "opencode");
        assert_eq!(detail.state.round, 2);
        assert_eq!(detail.state.rounds_in_window, Some(2));
        let context_sent = detail.state.context_sent.unwrap();
        assert_eq!(context_sent.get("opencode"), Some(&true));
        assert_eq!(context_sent.get("kimi"), Some(&true));
    }

    #[tokio::test]
    async fn uses_seed_session_id_on_first_run() {
        let root = TempDir::new().unwrap();
        let fake = write_fake(
            root.path(),
            "seed-actor.sh",
            r#"#!/bin/sh
[ "$BUDDY_MODE" = "resume" ] || { echo "mode $BUDDY_MODE" >&2; exit 1; }
[ "$BUDDY_SESSION_ID" = "seed-session" ] || { echo "session $BUDDY_SESSION_ID" >&2; exit 1; }
case " $* " in *" --session-id seed-session "*) ;; *) echo "args $*" >&2; exit 1;; esac
printf '{"type":"chat","content":"seeded output"}' > "$BUDDY_OUTPUT_FILE"
printf '%s\n' '{"type":"buddy.session","actor":"opencode","session_id":"next-session"}' > "$BUDDY_EVENT_FILE"
"#,
        )
        .await;
        let store = Arc::new(BuddyStore::new(root.path()));
        patch_global_settings(&store, json!({ "max_rounds": 1 })).await;
        let created = create_demo_task(
            &store,
            &root,
            settings_map(&[
                ("seed_opencode_session_id", json!("seed-session")),
                (
                    "launchers",
                    json!({ "opencode": { "command": fake, "env": {}, "timeout_seconds": 5 } }),
                ),
            ]),
        )
        .await;
        let runner = live_runner(&store);

        runner
            .start_task(
                "demo",
                StartTaskInput {
                    workspace_key: Some(created.workspace_key.clone()),
                    actor: Some("opencode".to_string()),
                    message: None,
                },
            )
            .await
            .unwrap();

        let detail = store
            .get_task_detail("demo", &created.workspace_key)
            .await
            .unwrap();
        assert_eq!(
            detail.state.opencode_session_id.as_deref(),
            Some("next-session")
        );
    }

    #[tokio::test]
    async fn pauses_after_run_reaching_max_rounds() {
        let root = TempDir::new().unwrap();
        let fake = write_fake(
            root.path(),
            "max-rounds-actor.sh",
            "#!/bin/sh\nprintf '{\"type\":\"chat\",\"content\":\"one round\"}' > \"$BUDDY_OUTPUT_FILE\"\n",
        )
        .await;
        let store = Arc::new(BuddyStore::new(root.path()));
        patch_global_settings(&store, json!({ "max_rounds": 1 })).await;
        let created = create_demo_task(
            &store,
            &root,
            settings_map(&[(
                "launchers",
                json!({ "claude": { "command": fake, "env": {}, "timeout_seconds": 5 } }),
            )]),
        )
        .await;
        let runner = live_runner(&store);

        runner
            .start_task(
                "demo",
                StartTaskInput {
                    workspace_key: Some(created.workspace_key.clone()),
                    actor: Some("claude".to_string()),
                    message: None,
                },
            )
            .await
            .unwrap();

        let detail = store
            .get_task_detail("demo", &created.workspace_key)
            .await
            .unwrap();
        assert_eq!(detail.state.status, TaskStatus::Paused);
        assert!(detail.state.countdown.is_none());
        assert_eq!(detail.state.rounds_in_window, Some(1));
        assert_eq!(detail.state.next_actor, "codex");
        let event = detail
            .events
            .iter()
            .find(|e| e.event_type == "round_window.paused")
            .expect("round_window.paused event");
        assert_eq!(event.payload.get("max_rounds"), Some(&json!(1)));
        assert_eq!(event.payload.get("rounds_in_window"), Some(&json!(1)));
        assert_eq!(event.payload.get("next_actor"), Some(&json!("codex")));
    }

    #[tokio::test]
    async fn kimi_native_run_captures_session_and_final_text() {
        let root = TempDir::new().unwrap();
        let fake = write_fake(
            root.path(),
            "kimi",
            r#"#!/bin/sh
cat >/dev/null
printf '%s\n' "{\"role\":\"assistant\",\"content\":\"{\\\"type\\\":\\\"chat\\\",\\\"content\\\":\\\"intermediate\\\"}\"}"
printf '%s\n' "{\"role\":\"meta\",\"type\":\"session.resume_hint\",\"session_id\":\"session_abc123-def456\",\"content\":\"To resume: kimi -r session_abc123-def456\"}"
printf '%s\n' "{\"role\":\"assistant\",\"content\":\"{\\\"type\\\":\\\"chat\\\",\\\"content\\\":\\\"final answer\\\"}\"}"
"#,
        )
        .await;
        let store = Arc::new(BuddyStore::new(root.path()));
        patch_global_settings(&store, json!({ "max_rounds": 1 })).await;
        let created = create_demo_task(
            &store,
            &root,
            settings_map(&[(
                "launchers",
                json!({ "kimi": { "command": fake, "env": {}, "timeout_seconds": 5 } }),
            )]),
        )
        .await;
        let runner = live_runner(&store);

        runner
            .start_task(
                "demo",
                StartTaskInput {
                    workspace_key: Some(created.workspace_key.clone()),
                    actor: Some("kimi".to_string()),
                    message: None,
                },
            )
            .await
            .unwrap();

        let detail = store
            .get_task_detail("demo", &created.workspace_key)
            .await
            .unwrap();
        assert_eq!(
            detail.state.kimi_session_id.as_deref(),
            Some("session_abc123-def456")
        );
        assert!(detail
            .transcript
            .iter()
            .any(|t| t.role == "kimi" && t.content == "final answer"));
        assert!(!detail
            .transcript
            .iter()
            .any(|t| t.role == "kimi" && t.content == "intermediate"));
    }

    #[tokio::test]
    async fn cursor_native_run_captures_stream_json_session() {
        let root = TempDir::new().unwrap();
        let fake = write_fake(
            root.path(),
            "cursor-agent",
            r#"#!/bin/sh
printf '%s\n' '{"type":"system","subtype":"init","session_id":"cursor-chat-123","model":"GPT-5"}'
printf '%s\n' '{"type":"assistant","session_id":"cursor-chat-123","message":{"role":"assistant","content":[{"type":"text","text":"{\"type\":\"chat\",\"content\":\"cursor final\"}"}]}}'
printf '%s\n' '{"type":"result","subtype":"success","session_id":"cursor-chat-123","result":"{\"type\":\"chat\",\"content\":\"cursor final\"}"}'
"#,
        )
        .await;
        let store = Arc::new(BuddyStore::new(root.path()));
        patch_global_settings(&store, json!({ "max_rounds": 1 })).await;
        let created = create_demo_task(
            &store,
            &root,
            settings_map(&[(
                "launchers",
                json!({ "cursor": { "command": fake, "env": {}, "timeout_seconds": 5 } }),
            )]),
        )
        .await;
        let runner = live_runner(&store);

        runner
            .start_task(
                "demo",
                StartTaskInput {
                    workspace_key: Some(created.workspace_key.clone()),
                    actor: Some("cursor".to_string()),
                    message: None,
                },
            )
            .await
            .unwrap();

        let detail = store
            .get_task_detail("demo", &created.workspace_key)
            .await
            .unwrap();
        assert_eq!(
            detail.state.cursor_session_id.as_deref(),
            Some("cursor-chat-123")
        );
        assert!(detail
            .transcript
            .iter()
            .any(|t| t.role == "cursor" && t.content == "cursor final"));
    }

    #[tokio::test]
    async fn dual_break_confirmation_reaches_done() {
        let root = TempDir::new().unwrap();
        let fake = write_fake(root.path(), "contract-break.sh", BREAK_ACTOR).await;
        let store = Arc::new(BuddyStore::new(root.path()));
        let created = create_demo_task(
            &store,
            &root,
            settings_map(&[(
                "launchers",
                json!({
                    "claude": { "command": fake, "env": {}, "timeout_seconds": 5 },
                    "codex": { "command": fake, "env": {}, "timeout_seconds": 5 }
                }),
            )]),
        )
        .await;
        let runner = live_runner(&store);

        // codex signals break → claude auto-started → claude confirms → DONE.
        runner
            .start_task(
                "demo",
                StartTaskInput {
                    workspace_key: Some(created.workspace_key.clone()),
                    actor: Some("codex".to_string()),
                    message: None,
                },
            )
            .await
            .unwrap();

        let detail = store
            .get_task_detail("demo", &created.workspace_key)
            .await
            .unwrap();
        assert_eq!(detail.state.status, TaskStatus::Done);
        assert_eq!(detail.transcript.len(), 4);

        assert_eq!(detail.transcript[0].role, "codex");
        assert_eq!(detail.transcript[0].content, "codex confirms done");
        let meta0 = detail.transcript[0].meta.as_ref().unwrap();
        assert_eq!(meta0.get("buddy_type"), Some(&json!("break")));
        assert_eq!(meta0.get("round"), Some(&json!(1)));

        assert_eq!(detail.transcript[1].role, "system");
        assert_eq!(
            detail.transcript[1].content,
            "Codex 请求结束任务，等待 Claude Code 确认。"
        );
        let meta1 = detail.transcript[1].meta.as_ref().unwrap();
        assert_eq!(meta1.get("kind"), Some(&json!("round_notice")));
        assert_eq!(meta1.get("round"), Some(&json!(1)));

        assert_eq!(detail.transcript[2].role, "claude");
        assert_eq!(detail.transcript[2].content, "claude confirms done");
        let meta2 = detail.transcript[2].meta.as_ref().unwrap();
        assert_eq!(meta2.get("buddy_type"), Some(&json!("break")));
        assert_eq!(meta2.get("round"), Some(&json!(2)));

        assert_eq!(detail.transcript[3].role, "system");
        assert_eq!(
            detail.transcript[3].content,
            "Codex 和 Claude Code 均确认任务完成，任务结束。"
        );
        let meta3 = detail.transcript[3].meta.as_ref().unwrap();
        assert_eq!(meta3.get("kind"), Some(&json!("round_notice")));
        assert_eq!(meta3.get("round"), Some(&json!(2)));
    }

    // -----------------------------------------------------------------------
    // buddy-runner-launcher.test.ts — queue terminal notifications
    // -----------------------------------------------------------------------

    async fn make_notifying_runner(
        root: &TempDir,
        script: &str,
    ) -> (
        Arc<BuddyStore>,
        crate::buddy::types::CreateTaskResult,
        BuddyRunner,
        Arc<Mutex<Vec<String>>>,
    ) {
        let fake = write_fake(root.path(), "actor.sh", script).await;
        let store = Arc::new(BuddyStore::new(root.path()));
        let created = create_demo_task(
            &store,
            root,
            settings_map(&[(
                "launchers",
                json!({
                    "claude": { "command": fake, "env": {}, "timeout_seconds": 5 },
                    "codex": { "command": fake, "env": {}, "timeout_seconds": 5 }
                }),
            )]),
        )
        .await;
        let runner = live_runner(&store);
        let calls = Arc::new(Mutex::new(Vec::<String>::new()));
        let calls_clone = calls.clone();
        runner.set_on_task_terminal(move |ws| calls_clone.lock().push(ws.to_string()));
        (store, created, runner, calls)
    }

    #[tokio::test]
    async fn does_not_notify_while_auto_advancing_rounds() {
        let root = TempDir::new().unwrap();
        let (store, created, runner, calls) = make_notifying_runner(&root, CHAT_ACTOR).await;
        patch_global_settings(&store, json!({ "max_rounds": 3 })).await;
        runner
            .start_task(
                "demo",
                StartTaskInput {
                    workspace_key: Some(created.workspace_key.clone()),
                    actor: Some("claude".to_string()),
                    message: None,
                },
            )
            .await
            .unwrap();
        assert!(calls.lock().len() <= 1);
        let detail = store
            .get_task_detail("demo", &created.workspace_key)
            .await
            .unwrap();
        assert!(detail.state.round >= 2);
    }

    #[tokio::test]
    async fn notifies_once_when_multi_round_task_reaches_done() {
        let root = TempDir::new().unwrap();
        let (_store, created, runner, calls) = make_notifying_runner(&root, BREAK_ACTOR).await;
        runner
            .start_task(
                "demo",
                StartTaskInput {
                    workspace_key: Some(created.workspace_key.clone()),
                    actor: Some("codex".to_string()),
                    message: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(calls.lock().as_slice(), &[created.workspace_key.clone()]);
    }

    #[tokio::test]
    async fn notifies_once_when_task_fails() {
        let root = TempDir::new().unwrap();
        let (store, created, runner, calls) = make_notifying_runner(&root, FAILING_ACTOR).await;
        patch_global_settings(&store, json!({ "max_consecutive_failures": 100 })).await;
        let result = runner
            .start_task(
                "demo",
                StartTaskInput {
                    workspace_key: Some(created.workspace_key.clone()),
                    actor: Some("claude".to_string()),
                    message: None,
                },
            )
            .await;
        assert!(result.is_err());
        assert_eq!(calls.lock().len(), 1);
    }

    #[tokio::test]
    async fn does_not_notify_again_when_auto_advance_unwinds() {
        let root = TempDir::new().unwrap();
        let (_store, created, runner, calls) = make_notifying_runner(&root, BREAK_ACTOR).await;
        runner
            .start_task(
                "demo",
                StartTaskInput {
                    workspace_key: Some(created.workspace_key.clone()),
                    actor: Some("codex".to_string()),
                    message: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(calls.lock().len(), 1);
    }

    // -----------------------------------------------------------------------
    // buddy-countdown.test.ts
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn pause_countdown_returns_task_to_ready() {
        let root = TempDir::new().unwrap();
        let store = Arc::new(BuddyStore::new(root.path()));
        let created = create_demo_task(&store, &root, None).await;
        store
            .update_task_state("demo", &created.workspace_key, |mut state| {
                state.status = TaskStatus::Countdown;
                state.countdown = Some(Countdown {
                    status: "running".to_string(),
                    remaining: Some(30),
                    started_at: None,
                    after_actor: None,
                    default_next_actor: "codex".to_string(),
                    deadline: None,
                });
                state
            })
            .await
            .unwrap();
        let runner = live_runner(&store);

        runner
            .pause_countdown(
                "demo",
                CountdownInput {
                    workspace_key: Some(created.workspace_key.clone()),
                    next_actor: Some("claude".to_string()),
                },
            )
            .await
            .unwrap();

        let detail = store
            .get_task_detail("demo", &created.workspace_key)
            .await
            .unwrap();
        assert_eq!(detail.state.status, TaskStatus::Ready);
        assert_eq!(detail.state.next_actor, "claude");
        assert_eq!(
            detail.state.countdown.as_ref().map(|c| c.status.as_str()),
            Some("paused")
        );
        let event = detail
            .events
            .iter()
            .find(|e| e.event_type == "countdown.paused")
            .expect("countdown.paused event");
        assert_eq!(event.payload.get("next_actor"), Some(&json!("claude")));
    }

    // -----------------------------------------------------------------------
    // buddy-exit-error.test.ts
    // -----------------------------------------------------------------------

    #[test]
    fn exit_error_message_covers_signal_code_and_neither() {
        assert_eq!(
            exit_error_message(None, Some("SIGTERM")),
            "Actor was killed by signal SIGTERM (possible timeout)"
        );
        assert_eq!(
            exit_error_message(None, None),
            "Actor exited unexpectedly (no exit code)"
        );
        assert_eq!(exit_error_message(Some(1), None), "Actor exited with code 1");
        assert_eq!(exit_error_message(Some(0), None), "Actor exited with code 0");
        assert_eq!(
            exit_error_message(None, Some("SIGKILL")),
            "Actor was killed by signal SIGKILL (possible timeout)"
        );
    }

    // -----------------------------------------------------------------------
    // osascript shim (opencode plugin notification hang fix)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn osascript_shim_is_written_executable_and_idempotent() {
        let root = TempDir::new().unwrap();
        let dir = ensure_osascript_shim_dir(root.path()).await.unwrap();
        let shim = dir.join("osascript");
        let content = tokio::fs::read_to_string(&shim).await.unwrap();
        assert_eq!(content, OSASCRIPT_SHIM);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&shim).unwrap().permissions().mode();
            assert_eq!(mode & 0o111, 0o111, "shim must be executable");
        }
        // Second call succeeds without rewriting (idempotent).
        let dir2 = ensure_osascript_shim_dir(root.path()).await.unwrap();
        assert_eq!(dir, dir2);
    }

    #[tokio::test]
    async fn osascript_shim_swallows_display_notification_but_passes_through() {
        let root = TempDir::new().unwrap();
        let dir = ensure_osascript_shim_dir(root.path()).await.unwrap();
        let shim = dir.join("osascript");

        // `display notification` exits 0 immediately (no real osascript, no
        // macOS automation consent dialog).
        let start = std::time::Instant::now();
        let out = tokio::process::Command::new(&shim)
            .args(["-e", r#"display notification "done" with title "OpenCode""#])
            .output()
            .await
            .unwrap();
        assert!(out.status.success());
        assert!(
            start.elapsed() < std::time::Duration::from_secs(15),
            "notification shim must not block on a consent dialog"
        );

        // Anything else is passed through to the real osascript.
        let out = tokio::process::Command::new(&shim)
            .args(["-e", "return 41"])
            .output()
            .await
            .unwrap();
        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "41");
    }

    #[test]
    fn prepend_path_puts_dir_first() {
        let mut env = HashMap::new();
        env.insert("PATH".to_string(), "/usr/bin:/bin".to_string());
        prepend_path(&mut env, Path::new("/shims"));
        assert_eq!(env.get("PATH").unwrap(), "/shims:/usr/bin:/bin");

        let mut env = HashMap::new();
        prepend_path(&mut env, Path::new("/shims"));
        assert!(
            env.get("PATH").unwrap().starts_with("/shims:"),
            "falls back to process PATH, got {:?}",
            env.get("PATH")
        );
    }


    // -----------------------------------------------------------------------
    // buddy-failure.test.ts
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn marks_task_failed_when_actor_exits_nonzero() {
        let root = TempDir::new().unwrap();
        let fake = write_fake(root.path(), "fake-fail.sh", FAILING_ACTOR).await;
        let store = Arc::new(BuddyStore::new(root.path()));
        let created = create_demo_task(
            &store,
            &root,
            settings_map(&[(
                "launchers",
                json!({ "codex": { "command": fake, "env": {}, "timeout_seconds": 5 } }),
            )]),
        )
        .await;
        let runner = live_runner(&store);

        let result = runner
            .start_task(
                "demo",
                StartTaskInput {
                    workspace_key: Some(created.workspace_key.clone()),
                    actor: Some("codex".to_string()),
                    message: None,
                },
            )
            .await;
        assert!(result.is_err());

        let detail = store
            .get_task_detail("demo", &created.workspace_key)
            .await
            .unwrap();
        assert_eq!(detail.state.status, TaskStatus::Failed);
        assert!(detail
            .latest_failure
            .as_ref()
            .map(|f| f.message.contains("boom"))
            .unwrap_or(false));
    }

    #[tokio::test]
    async fn retries_failed_actor_from_failed_state() {
        let root = TempDir::new().unwrap();
        let store = Arc::new(BuddyStore::new(root.path()));
        let created = create_demo_task(&store, &root, None).await;
        store
            .update_task_state("demo", &created.workspace_key, |mut state| {
                state.status = TaskStatus::Failed;
                state.next_actor = "claude".to_string();
                state.active_run = None;
                state.latest_failure = Some(Failure {
                    actor: Some("codex".to_string()),
                    message: "boom".to_string(),
                    run_id: None,
                    ts: Some("2026-05-26T07:00:00.000Z".to_string()),
                    output_file: None,
                    event_file: None,
                });
                state
            })
            .await
            .unwrap();
        let runner = deferred_runner(&store);

        runner
            .start_task(
                "demo",
                StartTaskInput {
                    workspace_key: Some(created.workspace_key.clone()),
                    actor: None,
                    message: None,
                },
            )
            .await
            .unwrap();

        let detail = store
            .get_task_detail("demo", &created.workspace_key)
            .await
            .unwrap();
        assert_eq!(detail.state.status, TaskStatus::RunningCodex);
        assert_eq!(
            detail.state.active_run.as_ref().map(|r| r.actor.as_str()),
            Some("codex")
        );
        assert!(detail.latest_failure.is_none());
    }

    #[tokio::test]
    async fn detects_error_only_output_even_with_zero_exit() {
        let root = TempDir::new().unwrap();
        let fake = write_fake(
            root.path(),
            "fake-error-output.sh",
            "#!/bin/sh\nprintf '%s\\n' '{\"type\":\"error\",\"error\":{\"name\":\"APIError\",\"data\":{\"message\":\"Subscription expired\",\"statusCode\":400}}}'\nexit 0\n",
        )
        .await;
        let store = Arc::new(BuddyStore::new(root.path()));
        let created = create_demo_task(
            &store,
            &root,
            settings_map(&[(
                "launchers",
                json!({ "opencode": { "command": fake, "env": {}, "timeout_seconds": 5 } }),
            )]),
        )
        .await;
        let runner = live_runner(&store);

        let result = runner
            .start_task(
                "demo",
                StartTaskInput {
                    workspace_key: Some(created.workspace_key.clone()),
                    actor: Some("opencode".to_string()),
                    message: None,
                },
            )
            .await;
        assert!(result.is_err());

        let detail = store
            .get_task_detail("demo", &created.workspace_key)
            .await
            .unwrap();
        assert_eq!(detail.state.status, TaskStatus::Failed);
        assert_eq!(detail.state.consecutive_failures, Some(1));
    }

    #[tokio::test]
    async fn auto_confirms_pending_break_when_other_actor_fails() {
        let root = TempDir::new().unwrap();
        let fake = write_fake(
            root.path(),
            "fake-fail-break.sh",
            "#!/bin/sh\nprintf 'API error\\n' >&2\nexit 1\n",
        )
        .await;
        let store = Arc::new(BuddyStore::new(root.path()));
        let created = create_demo_task(
            &store,
            &root,
            settings_map(&[(
                "launchers",
                json!({ "opencode": { "command": fake, "env": {}, "timeout_seconds": 5 } }),
            )]),
        )
        .await;
        store
            .update_task_state("demo", &created.workspace_key, |mut state| {
                state.pending_break = Some(BreakMarker {
                    actor: Some("claude".to_string()),
                    round: Some(5),
                });
                state
            })
            .await
            .unwrap();
        let runner = live_runner(&store);

        let result = runner
            .start_task(
                "demo",
                StartTaskInput {
                    workspace_key: Some(created.workspace_key.clone()),
                    actor: Some("opencode".to_string()),
                    message: None,
                },
            )
            .await;
        assert!(result.is_err());

        let detail = store
            .get_task_detail("demo", &created.workspace_key)
            .await
            .unwrap();
        assert_eq!(detail.state.status, TaskStatus::Done);
        assert!(detail.state.pending_break.is_none());
        let done_event = detail
            .events
            .iter()
            .find(|e| e.event_type == "task.done")
            .expect("task.done event");
        assert_eq!(
            done_event.payload.get("reason"),
            Some(&json!("break_confirmed_on_failure"))
        );
    }

    #[tokio::test]
    async fn pauses_when_consecutive_failures_reach_threshold() {
        let root = TempDir::new().unwrap();
        let fake = write_fake(
            root.path(),
            "fake-fail-max.sh",
            "#!/bin/sh\nprintf 'fail\\n' >&2\nexit 1\n",
        )
        .await;
        let store = Arc::new(BuddyStore::new(root.path()));
        let created = create_demo_task(
            &store,
            &root,
            settings_map(&[(
                "launchers",
                json!({ "codex": { "command": fake, "env": {}, "timeout_seconds": 5 } }),
            )]),
        )
        .await;
        store
            .update_task_state("demo", &created.workspace_key, |mut state| {
                state.consecutive_failures = Some(9);
                state.status = TaskStatus::Failed;
                state.active_run = None;
                state.latest_failure = Some(Failure {
                    actor: Some("codex".to_string()),
                    message: "previous fail".to_string(),
                    run_id: None,
                    ts: Some(utc_now()),
                    output_file: None,
                    event_file: None,
                });
                state
            })
            .await
            .unwrap();
        let runner = live_runner(&store);

        let result = runner
            .start_task(
                "demo",
                StartTaskInput {
                    workspace_key: Some(created.workspace_key.clone()),
                    actor: Some("codex".to_string()),
                    message: None,
                },
            )
            .await;
        assert!(result.is_err());

        let detail = store
            .get_task_detail("demo", &created.workspace_key)
            .await
            .unwrap();
        assert_eq!(detail.state.status, TaskStatus::Paused);
        assert_eq!(detail.state.consecutive_failures, Some(10));
        assert!(event_types(&detail).contains(&"failure_threshold.reached"));
    }

    #[tokio::test]
    async fn no_auto_confirm_when_failing_actor_holds_pending_break() {
        let root = TempDir::new().unwrap();
        let fake = write_fake(
            root.path(),
            "fake-fail-same.sh",
            "#!/bin/sh\nprintf 'fail\\n' >&2\nexit 1\n",
        )
        .await;
        let store = Arc::new(BuddyStore::new(root.path()));
        let created = create_demo_task(
            &store,
            &root,
            settings_map(&[(
                "launchers",
                json!({ "opencode": { "command": fake, "env": {}, "timeout_seconds": 5 } }),
            )]),
        )
        .await;
        store
            .update_task_state("demo", &created.workspace_key, |mut state| {
                state.pending_break = Some(BreakMarker {
                    actor: Some("opencode".to_string()),
                    round: Some(5),
                });
                state
            })
            .await
            .unwrap();
        let runner = live_runner(&store);

        let result = runner
            .start_task(
                "demo",
                StartTaskInput {
                    workspace_key: Some(created.workspace_key.clone()),
                    actor: Some("opencode".to_string()),
                    message: None,
                },
            )
            .await;
        assert!(result.is_err());

        let detail = store
            .get_task_detail("demo", &created.workspace_key)
            .await
            .unwrap();
        assert_eq!(detail.state.status, TaskStatus::Failed);
    }

    // -----------------------------------------------------------------------
    // buddy-recovery.test.ts (service-level in TS; the per-task recovery lives
    // on the runner here — see recover_interrupted_runs docs)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn recover_interrupted_runs_marks_active_runs_paused() {
        let root = TempDir::new().unwrap();
        let store = Arc::new(BuddyStore::new(root.path()));
        let created = create_demo_task(&store, &root, None).await;
        store
            .update_task_state("demo", &created.workspace_key, |mut state| {
                state.status = TaskStatus::RunningCodex;
                state.active_run = Some(ActiveRun {
                    run_id: None,
                    actor: "codex".to_string(),
                    started_at: "2026-05-26T00:00:00.000Z".to_string(),
                    status: None,
                    session_id_before: None,
                    session_id_after: None,
                });
                state
            })
            .await
            .unwrap();
        let runner = live_runner(&store);

        runner.recover_interrupted_runs().await.unwrap();

        let detail = store
            .get_task_detail("demo", &created.workspace_key)
            .await
            .unwrap();
        assert_eq!(detail.state.status, TaskStatus::Paused);
        assert!(detail.state.active_run.is_none());
    }

    // -----------------------------------------------------------------------
    // buddy-upgrade-retry.test.ts
    // -----------------------------------------------------------------------

    #[test]
    fn upgrade_exit_error_detects_english_messages() {
        assert!(is_upgrade_exit_error(
            "A new version is available. Update complete, please restart."
        ));
        assert!(is_upgrade_exit_error("Upgrade complete, restarting..."));
        assert!(is_upgrade_exit_error("Auto-update in progress"));
        assert!(is_upgrade_exit_error("Updated to v2.0.0, restart required"));
    }

    #[test]
    fn upgrade_exit_error_detects_chinese_messages() {
        assert!(is_upgrade_exit_error("检测到新版本，自动更新中..."));
        assert!(is_upgrade_exit_error("自动升级完成，请重启"));
        assert!(is_upgrade_exit_error("已更新到最新版本"));
        assert!(is_upgrade_exit_error("升级完成"));
    }

    #[test]
    fn upgrade_exit_error_ignores_unrelated_errors() {
        assert!(!is_upgrade_exit_error("Connection refused"));
        assert!(!is_upgrade_exit_error("Permission denied"));
        assert!(!is_upgrade_exit_error("Actor exited with code 1"));
        assert!(!is_upgrade_exit_error("Command not found"));
        assert!(!is_upgrade_exit_error(""));
    }

    const UPGRADE_STDERR_ACTOR: &str = "#!/bin/sh\nprintf '%s\\n' 'A new version is available. Upgrade complete, restart required.' >&2\nexit 1\n";
    const UPGRADE_STDOUT_ACTOR: &str = "#!/bin/sh\nprintf '%s\\n' 'A new version is available. Upgrade complete, restart required.'\nexit 1\n";
    const NORMAL_ERROR_ACTOR: &str = "#!/bin/sh\nprintf '%s\\n' 'Some random runtime error' >&2\nexit 1\n";

    async fn make_upgrade_task(
        root: &TempDir,
        script: &str,
        global_patch: Value,
        session_id: &str,
    ) -> (Arc<BuddyStore>, crate::buddy::types::CreateTaskResult) {
        let fake = write_fake(root.path(), "fake-upgrade.sh", script).await;
        let store = Arc::new(BuddyStore::new(root.path()));
        patch_global_settings(&store, global_patch).await;
        let created = create_demo_task(
            &store,
            root,
            settings_map(&[(
                "launchers",
                json!({ "claude": { "command": fake, "env": {}, "timeout_seconds": 5 } }),
            )]),
        )
        .await;
        let session = session_id.to_string();
        store
            .update_task_state("demo", &created.workspace_key, move |mut state| {
                state.claude_session_id = Some(session.clone());
                state
            })
            .await
            .unwrap();
        (store, created)
    }

    #[tokio::test]
    async fn upgrade_exit_retries_then_fails_after_max_retries() {
        let root = TempDir::new().unwrap();
        let (store, created) = make_upgrade_task(
            &root,
            UPGRADE_STDERR_ACTOR,
            json!({ "max_upgrade_retries": 1, "max_compact_retries": 0 }),
            "upgrade-test-session",
        )
        .await;
        let runner = live_runner(&store);

        let result = runner
            .start_task(
                "demo",
                StartTaskInput {
                    workspace_key: Some(created.workspace_key.clone()),
                    actor: Some("claude".to_string()),
                    message: None,
                },
            )
            .await;
        assert!(result.is_err());

        let detail = store
            .get_task_detail("demo", &created.workspace_key)
            .await
            .unwrap();
        let upgrade_events: Vec<_> = detail
            .events
            .iter()
            .filter(|e| e.event_type == "actor.upgrade_detected")
            .collect();
        assert!(!upgrade_events.is_empty());
        assert_eq!(upgrade_events[0].payload.get("retry_attempt"), Some(&json!(1)));

        let retry_transcript = detail
            .transcript
            .iter()
            .find(|t| {
                t.meta
                    .as_ref()
                    .and_then(|m| m.get("kind"))
                    .map(|k| k == &json!("upgrade_retry"))
                    .unwrap_or(false)
            })
            .expect("upgrade_retry transcript");
        assert!(retry_transcript.content.contains("自动升级"));
        assert_eq!(detail.state.status, TaskStatus::Failed);
    }

    #[tokio::test]
    async fn upgrade_exit_not_retried_when_max_retries_zero() {
        let root = TempDir::new().unwrap();
        let (store, created) = make_upgrade_task(
            &root,
            UPGRADE_STDERR_ACTOR,
            json!({ "max_upgrade_retries": 0, "max_compact_retries": 0 }),
            "upgrade-test-session-2",
        )
        .await;
        let runner = live_runner(&store);

        let result = runner
            .start_task(
                "demo",
                StartTaskInput {
                    workspace_key: Some(created.workspace_key.clone()),
                    actor: Some("claude".to_string()),
                    message: None,
                },
            )
            .await;
        assert!(result.is_err());

        let detail = store
            .get_task_detail("demo", &created.workspace_key)
            .await
            .unwrap();
        assert!(!event_types(&detail).contains(&"actor.upgrade_detected"));
        assert_eq!(detail.state.status, TaskStatus::Failed);
    }

    #[tokio::test]
    async fn non_upgrade_errors_are_not_retried_as_upgrades() {
        let root = TempDir::new().unwrap();
        let (store, created) = make_upgrade_task(
            &root,
            NORMAL_ERROR_ACTOR,
            json!({ "max_upgrade_retries": 3, "max_compact_retries": 0 }),
            "normal-error-session",
        )
        .await;
        let runner = live_runner(&store);

        let result = runner
            .start_task(
                "demo",
                StartTaskInput {
                    workspace_key: Some(created.workspace_key.clone()),
                    actor: Some("claude".to_string()),
                    message: None,
                },
            )
            .await;
        assert!(result.is_err());

        let detail = store
            .get_task_detail("demo", &created.workspace_key)
            .await
            .unwrap();
        assert!(!event_types(&detail).contains(&"actor.upgrade_detected"));
        assert_eq!(detail.state.status, TaskStatus::Failed);
    }

    #[tokio::test]
    async fn upgrade_exit_detected_on_stdout() {
        let root = TempDir::new().unwrap();
        let (store, created) = make_upgrade_task(
            &root,
            UPGRADE_STDOUT_ACTOR,
            json!({ "max_upgrade_retries": 1, "max_compact_retries": 0 }),
            "upgrade-stdout-session",
        )
        .await;
        let runner = live_runner(&store);

        let result = runner
            .start_task(
                "demo",
                StartTaskInput {
                    workspace_key: Some(created.workspace_key.clone()),
                    actor: Some("claude".to_string()),
                    message: None,
                },
            )
            .await;
        assert!(result.is_err());

        let detail = store
            .get_task_detail("demo", &created.workspace_key)
            .await
            .unwrap();
        let upgrade_events: Vec<_> = detail
            .events
            .iter()
            .filter(|e| e.event_type == "actor.upgrade_detected")
            .collect();
        assert!(!upgrade_events.is_empty());
        assert_eq!(detail.state.status, TaskStatus::Failed);
    }

    /// Fake CLI that exits to auto-upgrade on the first invocation per actor,
    /// then succeeds (valid buddy chat + claude-style init session line).
    const UPGRADE_ONCE_ACTOR: &str = r#"#!/bin/sh
actor="${BUDDY_ACTOR:-default}"
counter="$BUDDY_COUNTER_DIR/ping-$actor.cnt"
n=0
if [ -f "$counter" ]; then n=$(cat "$counter"); fi
n=$((n + 1))
printf '%s' "$n" > "$counter"
if [ "$n" -le 1 ]; then
  printf '%s\n' 'A new version is available. Upgrade complete, restart required.' >&2
  exit 1
fi
printf '%s\n' "{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"sid-$actor\"}"
if [ -n "$BUDDY_OUTPUT_FILE" ]; then
  printf '{"type":"chat","content":"ready"}' > "$BUDDY_OUTPUT_FILE"
fi
exit 0
"#;

    #[tokio::test]
    async fn health_check_ping_upgrade_retry_then_fails() {
        let root = TempDir::new().unwrap();
        let fake = write_fake(root.path(), "fake-upgrade.sh", UPGRADE_STDERR_ACTOR).await;
        let store = Arc::new(BuddyStore::new(root.path()));
        patch_global_settings(
            &store,
            json!({ "max_upgrade_retries": 1, "max_compact_retries": 0 }),
        )
        .await;
        let created = create_demo_task(
            &store,
            &root,
            settings_map(&[
                ("role_mode", json!("claude_implements")),
                (
                    "launchers",
                    json!({
                        "claude": { "command": fake, "env": {}, "timeout_seconds": 5 },
                        "codex": { "command": fake, "env": {}, "timeout_seconds": 5 }
                    }),
                ),
            ]),
        )
        .await;
        let runner = live_runner(&store);

        let result = runner
            .start_task(
                "demo",
                StartTaskInput {
                    workspace_key: Some(created.workspace_key.clone()),
                    actor: None,
                    message: None,
                },
            )
            .await;
        assert!(result.is_err());

        let detail = store
            .get_task_detail("demo", &created.workspace_key)
            .await
            .unwrap();
        let retry_events: Vec<_> = detail
            .events
            .iter()
            .filter(|e| e.event_type == "health_check.actor_upgrade_retry")
            .collect();
        assert!(!retry_events.is_empty());
        assert_eq!(retry_events[0].payload.get("retry_attempt"), Some(&json!(1)));

        let retry_transcript = detail
            .transcript
            .iter()
            .find(|t| {
                t.meta
                    .as_ref()
                    .and_then(|m| m.get("kind"))
                    .map(|k| k == &json!("health_check_upgrade_retry"))
                    .unwrap_or(false)
            })
            .expect("health_check_upgrade_retry transcript");
        assert!(retry_transcript.content.contains("自动升级"));

        assert_eq!(detail.state.status, TaskStatus::Failed);
        assert!(detail
            .state
            .health_check
            .as_ref()
            .and_then(|h| h.failed_actor.as_deref())
            .is_some());
    }

    #[tokio::test]
    async fn health_check_recovers_when_upgrade_succeeds_on_retry() {
        let root = TempDir::new().unwrap();
        let fake = write_fake(root.path(), "fake-upgrade-once.sh", UPGRADE_ONCE_ACTOR).await;
        let store = Arc::new(BuddyStore::new(root.path()));
        // max_rounds: 1 stops the buddy loop after the first actor round (the
        // fake always chats and never breaks).
        patch_global_settings(
            &store,
            json!({ "max_upgrade_retries": 2, "max_compact_retries": 0, "max_rounds": 1 }),
        )
        .await;
        let counter_dir = root.path().to_string_lossy().to_string();
        let created = create_demo_task(
            &store,
            &root,
            settings_map(&[
                ("role_mode", json!("claude_implements")),
                (
                    "launchers",
                    json!({
                        "claude": { "command": fake, "env": { "BUDDY_COUNTER_DIR": counter_dir }, "timeout_seconds": 5 },
                        "codex": { "command": fake, "env": { "BUDDY_COUNTER_DIR": counter_dir }, "timeout_seconds": 5 }
                    }),
                ),
            ]),
        )
        .await;
        let runner = live_runner(&store);

        runner
            .start_task(
                "demo",
                StartTaskInput {
                    workspace_key: Some(created.workspace_key.clone()),
                    actor: None,
                    message: None,
                },
            )
            .await
            .unwrap();

        let detail = store
            .get_task_detail("demo", &created.workspace_key)
            .await
            .unwrap();
        let retry_events: Vec<_> = detail
            .events
            .iter()
            .filter(|e| e.event_type == "health_check.actor_upgrade_retry")
            .collect();
        assert!(!retry_events.is_empty());
        let passed_events: Vec<_> = detail
            .events
            .iter()
            .filter(|e| e.event_type == "health_check.passed")
            .collect();
        assert_eq!(passed_events.len(), 1);
        assert!(detail.state.health_check.is_none());
        assert_eq!(
            detail.state.claude_session_id.as_deref(),
            Some("sid-claude")
        );
    }

    // -----------------------------------------------------------------------
    // buddy-compact-settings.test.ts (store-level; ported here per wave plan)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn max_compact_retries_zero_persists() {
        let root = TempDir::new().unwrap();
        let store = BuddyStore::new(root.path());
        patch_global_settings(&store, json!({ "max_compact_retries": 0 })).await;
        let read = store.read_global_settings().await.unwrap();
        assert_eq!(read.max_compact_retries, Some(0));
    }

    #[tokio::test]
    async fn max_compact_retries_five_persists() {
        let root = TempDir::new().unwrap();
        let store = BuddyStore::new(root.path());
        patch_global_settings(&store, json!({ "max_compact_retries": 5 })).await;
        let read = store.read_global_settings().await.unwrap();
        assert_eq!(read.max_compact_retries, Some(5));
    }

    #[tokio::test]
    async fn max_compact_retries_defaults_to_three() {
        let root = TempDir::new().unwrap();
        let store = BuddyStore::new(root.path());
        let read = store.read_global_settings().await.unwrap();
        assert_eq!(read.max_compact_retries, Some(3));
    }

    // -----------------------------------------------------------------------
    // buddy-compact-retry.test.ts
    // -----------------------------------------------------------------------

    #[test]
    fn context_window_limit_error_detection() {
        // English variants
        assert!(is_context_window_limit_error(
            "API Error: The model has reached its context window limit."
        ));
        assert!(is_context_window_limit_error("Error: context length exceeded"));
        assert!(is_context_window_limit_error(
            "This model maximum context length is 128000 tokens"
        ));
        assert!(is_context_window_limit_error("Token limit exceeded"));
        assert!(is_context_window_limit_error("too many tokens in request"));
        assert!(is_context_window_limit_error("Input exceeds token limit"));
        assert!(is_context_window_limit_error("Request exceeded token limit"));
        assert!(is_context_window_limit_error("Input too long for model"));
        assert!(is_context_window_limit_error("Request too large"));
        assert!(is_context_window_limit_error(
            "Actor exited with only noise events (likely context window exhausted)"
        ));
        assert!(is_context_window_limit_error(
            "Actor produced only noise events (context window likely exhausted): ..."
        ));
        // Case-insensitive
        assert!(is_context_window_limit_error("CONTEXT WINDOW LIMIT"));
        assert!(is_context_window_limit_error("Context Length Exceeded"));
        // Chinese variants
        assert!(is_context_window_limit_error(
            "对话内容太长，已超出当前模型的处理能力。请新建对话，或换用支持更长上下文的模型继续。"
        ));
        assert!(is_context_window_limit_error("超出当前模型的处理能力"));
        assert!(is_context_window_limit_error("上下文超限"));
        assert!(is_context_window_limit_error("上下文超出限制"));
        assert!(is_context_window_limit_error("超出模型的最大长度"));
        assert!(is_context_window_limit_error("内容过长，请缩短输入"));
        assert!(is_context_window_limit_error(
            "API Error: 400 {\"error\":{\"message\":\"对话内容太长，已超出当前模型的处理能力。\"},\"type\":\"error\"}"
        ));
        // Unrelated
        assert!(!is_context_window_limit_error("Connection refused"));
        assert!(!is_context_window_limit_error("Permission denied"));
        assert!(!is_context_window_limit_error("Actor exited with code 1"));
        assert!(!is_context_window_limit_error("Command not found"));
        assert!(!is_context_window_limit_error(""));
    }

    async fn make_context_limit_task(
        root: &TempDir,
        session_id: Option<&str>,
        global_patch: Option<Value>,
    ) -> (Arc<BuddyStore>, crate::buddy::types::CreateTaskResult) {
        let fake = write_fake(root.path(), "fake-ctx-limit.sh", CONTEXT_LIMIT_ACTOR).await;
        let store = Arc::new(BuddyStore::new(root.path()));
        if let Some(patch) = global_patch {
            patch_global_settings(&store, patch).await;
        }
        let created = create_demo_task(
            &store,
            root,
            settings_map(&[(
                "launchers",
                json!({ "claude": { "command": fake, "env": {}, "timeout_seconds": 5 } }),
            )]),
        )
        .await;
        if let Some(session_id) = session_id {
            let session = session_id.to_string();
            store
                .update_task_state("demo", &created.workspace_key, move |mut state| {
                    state.claude_session_id = Some(session.clone());
                    state
                })
                .await
                .unwrap();
        }
        (store, created)
    }

    async fn run_failing_claude(store: &Arc<BuddyStore>, workspace_key: &str) {
        let runner = live_runner(store);
        let result = runner
            .start_task(
                "demo",
                StartTaskInput {
                    workspace_key: Some(workspace_key.to_string()),
                    actor: Some("claude".to_string()),
                    message: None,
                },
            )
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn context_limit_detected_and_session_reset() {
        let root = TempDir::new().unwrap();
        let (store, created) =
            make_context_limit_task(&root, Some("test-session-123"), None).await;
        run_failing_claude(&store, &created.workspace_key).await;

        let detail = store
            .get_task_detail("demo", &created.workspace_key)
            .await
            .unwrap();
        let context_event = detail
            .events
            .iter()
            .find(|e| e.event_type == "actor.context_limit_detected")
            .expect("actor.context_limit_detected event");
        assert!(context_event
            .payload
            .get("error")
            .and_then(Value::as_str)
            .map(|e| e.contains("context window limit"))
            .unwrap_or(false));

        // We never attempt /compact.
        assert!(!event_types(&detail).contains(&"actor.compact_succeeded"));
        assert!(!event_types(&detail).contains(&"actor.compact_failed"));

        let reset_event = detail
            .events
            .iter()
            .find(|e| e.event_type == "actor.session_reset")
            .expect("actor.session_reset event");
        assert_eq!(
            reset_event.payload.get("reason"),
            Some(&json!("context_window_limit"))
        );

        assert_eq!(detail.state.status, TaskStatus::Failed);

        let reset_transcript = detail
            .transcript
            .iter()
            .find(|t| {
                t.meta
                    .as_ref()
                    .and_then(|m| m.get("kind"))
                    .map(|k| k == &json!("session_reset"))
                    .unwrap_or(false)
            })
            .expect("session_reset transcript");
        assert!(reset_transcript.content.contains("重置会话"));
    }

    #[tokio::test]
    async fn no_session_reset_without_session_id() {
        let root = TempDir::new().unwrap();
        let (store, created) = make_context_limit_task(&root, None, None).await;
        run_failing_claude(&store, &created.workspace_key).await;

        let detail = store
            .get_task_detail("demo", &created.workspace_key)
            .await
            .unwrap();
        assert!(!event_types(&detail).contains(&"actor.context_limit_detected"));
        assert_eq!(detail.state.status, TaskStatus::Failed);
    }

    #[tokio::test]
    async fn no_session_reset_for_non_context_errors() {
        let root = TempDir::new().unwrap();
        let fake = write_fake(
            root.path(),
            "fake-normal-error.sh",
            "#!/bin/sh\nprintf '%s\\n' 'Some other error' >&2\nexit 1\n",
        )
        .await;
        let store = Arc::new(BuddyStore::new(root.path()));
        let created = create_demo_task(
            &store,
            &root,
            settings_map(&[(
                "launchers",
                json!({ "claude": { "command": fake, "env": {}, "timeout_seconds": 5 } }),
            )]),
        )
        .await;
        store
            .update_task_state("demo", &created.workspace_key, |mut state| {
                state.claude_session_id = Some("test-session-456".to_string());
                state
            })
            .await
            .unwrap();
        run_failing_claude(&store, &created.workspace_key).await;

        let detail = store
            .get_task_detail("demo", &created.workspace_key)
            .await
            .unwrap();
        assert!(!event_types(&detail).contains(&"actor.context_limit_detected"));
        assert_eq!(detail.state.status, TaskStatus::Failed);
    }

    #[tokio::test]
    async fn respects_max_compact_retries_zero() {
        let root = TempDir::new().unwrap();
        let (store, created) = make_context_limit_task(
            &root,
            Some("test-session-789"),
            Some(json!({ "max_compact_retries": 0 })),
        )
        .await;
        run_failing_claude(&store, &created.workspace_key).await;

        let detail = store
            .get_task_detail("demo", &created.workspace_key)
            .await
            .unwrap();
        assert!(!event_types(&detail).contains(&"actor.context_limit_detected"));
        assert_eq!(detail.state.status, TaskStatus::Failed);
    }

    #[tokio::test]
    async fn session_reset_clears_session_id() {
        let root = TempDir::new().unwrap();
        let (store, created) =
            make_context_limit_task(&root, Some("session-to-be-cleared"), None).await;
        run_failing_claude(&store, &created.workspace_key).await;

        let detail = store
            .get_task_detail("demo", &created.workspace_key)
            .await
            .unwrap();
        let reset_event = detail
            .events
            .iter()
            .find(|e| e.event_type == "actor.session_reset")
            .expect("actor.session_reset event");
        assert_eq!(
            reset_event.payload.get("reason"),
            Some(&json!("context_window_limit"))
        );
        assert!(detail
            .transcript
            .iter()
            .any(|t| t
                .meta
                .as_ref()
                .and_then(|m| m.get("kind"))
                .map(|k| k == &json!("session_reset"))
                .unwrap_or(false)
                && t.content.contains("重置会话")));
        assert!(detail.state.claude_session_id.is_none());
    }

    #[tokio::test]
    async fn session_reset_writes_compact_context() {
        let root = TempDir::new().unwrap();
        let (store, created) =
            make_context_limit_task(&root, Some("test-session-summary"), None).await;
        run_failing_claude(&store, &created.workspace_key).await;

        let task_dir = store.task_directory("demo", &created.workspace_key);
        let content = tokio::fs::read_to_string(task_dir.join("context.md"))
            .await
            .unwrap();
        assert!(content.contains("上下文窗口限制已重置"));
        assert!(content.contains("请基于以上摘要继续工作"));
        assert!(content.len() < 5000);
    }

    #[tokio::test]
    async fn goes_directly_to_session_reset_without_compact() {
        let root = TempDir::new().unwrap();
        let (store, created) =
            make_context_limit_task(&root, Some("test-session-direct"), None).await;
        run_failing_claude(&store, &created.workspace_key).await;

        let detail = store
            .get_task_detail("demo", &created.workspace_key)
            .await
            .unwrap();
        let compact_events: Vec<_> = detail
            .events
            .iter()
            .filter(|e| {
                matches!(
                    e.event_type.as_str(),
                    "actor.compact_succeeded" | "actor.compact_failed" | "actor.compact_output"
                )
            })
            .collect();
        assert!(compact_events.is_empty());
        let compact_retry_transcripts: Vec<_> = detail
            .transcript
            .iter()
            .filter(|t| {
                t.meta
                    .as_ref()
                    .and_then(|m| m.get("kind"))
                    .map(|k| k == &json!("compact_retry"))
                    .unwrap_or(false)
            })
            .collect();
        assert!(compact_retry_transcripts.is_empty());
        let reset_transcripts: Vec<_> = detail
            .transcript
            .iter()
            .filter(|t| {
                t.meta
                    .as_ref()
                    .and_then(|m| m.get("kind"))
                    .map(|k| k == &json!("session_reset"))
                    .unwrap_or(false)
            })
            .collect();
        assert!(!reset_transcripts.is_empty());
    }

    #[tokio::test]
    async fn summarize_failure_falls_back_to_truncation() {
        let root = TempDir::new().unwrap();
        let (store, created) =
            make_context_limit_task(&root, Some("test-session-summarize"), None).await;
        run_failing_claude(&store, &created.workspace_key).await;

        let detail = store
            .get_task_detail("demo", &created.workspace_key)
            .await
            .unwrap();
        assert!(event_types(&detail).contains(&"actor.summarize_failed"));
        let reset_event = detail
            .events
            .iter()
            .find(|e| e.event_type == "actor.session_reset")
            .expect("actor.session_reset event");
        assert_eq!(
            reset_event.payload.get("summary_method"),
            Some(&json!("truncation"))
        );

        let task_dir = store.task_directory("demo", &created.workspace_key);
        let content = tokio::fs::read_to_string(task_dir.join("context.md"))
            .await
            .unwrap();
        assert!(content.contains("上下文窗口限制已重置"));
    }

    #[tokio::test]
    async fn summarize_attempt_events_recorded_with_fallback() {
        let root = TempDir::new().unwrap();
        let (store, created) =
            make_context_limit_task(&root, Some("test-session-events"), None).await;
        run_failing_claude(&store, &created.workspace_key).await;

        let detail = store
            .get_task_detail("demo", &created.workspace_key)
            .await
            .unwrap();
        assert!(event_types(&detail).contains(&"actor.summarize_failed"));
        let reset_event = detail
            .events
            .iter()
            .find(|e| e.event_type == "actor.session_reset")
            .expect("actor.session_reset event");
        assert_eq!(
            reset_event.payload.get("summary_method"),
            Some(&json!("truncation"))
        );
    }

    // -----------------------------------------------------------------------
    // buddy-health-check.test.ts
    // -----------------------------------------------------------------------

    fn health_state(overrides: Value) -> TaskState {
        let mut value = json!({ "status": "READY", "round": 0, "next_actor": "claude" });
        for (key, val) in overrides.as_object().unwrap() {
            value.as_object_mut().unwrap().insert(key.clone(), val.clone());
        }
        serde_json::from_value(value).unwrap()
    }

    fn health_settings(overrides: Value) -> TaskSettings {
        let mut value = json!({
            "protocol_version": "1",
            "flow_policy": "alternating",
            "role_mode": "claude_implements",
            "launchers": {}
        });
        for (key, val) in overrides.as_object().unwrap() {
            value.as_object_mut().unwrap().insert(key.clone(), val.clone());
        }
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn health_check_required_for_fresh_task() {
        assert!(needs_health_check(
            &health_state(json!({})),
            &health_settings(json!({}))
        ));
    }

    #[test]
    fn health_check_not_rerun_after_clean_pass() {
        let state = health_state(json!({
            "health_check": { "actors": { "claude": "passed", "codex": "passed" } }
        }));
        assert!(!needs_health_check(&state, &health_settings(json!({}))));
    }

    #[test]
    fn health_check_rerun_after_failure() {
        let state = health_state(json!({
            "status": "FAILED",
            "health_check": {
                "actors": { "claude": "passed", "codex": "failed" },
                "failed_actor": "codex",
                "failed_reason": "CLI not found"
            }
        }));
        assert!(needs_health_check(&state, &health_settings(json!({}))));
    }

    #[test]
    fn health_check_skipped_after_round_zero() {
        let state = health_state(json!({ "round": 1 }));
        assert!(!needs_health_check(&state, &health_settings(json!({}))));
    }

    #[test]
    fn health_check_skipped_with_seed_session() {
        let state = health_state(json!({ "claude_session_id": "seed-123" }));
        assert!(!needs_health_check(&state, &health_settings(json!({}))));
    }
}
