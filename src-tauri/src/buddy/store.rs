//! Filesystem store, port of `src/main/buddy/store.ts`.
//!
//! Same on-disk layout as the Electron edition:
//! `<dataRoot>/workspaces/<key>/tasks/<id>/{state.json,settings.json,task.md,
//! context.md,events.jsonl,transcript.jsonl,status,.buddy.lock}` and
//! `<dataRoot>/global/settings.json`. All writes go to `<file>.tmp-*` and are
//! then renamed into place (atomic write convention).

use crate::buddy::defaults::{
    default_launcher_for, normalize_global_settings, DEFAULT_LAUNCHER_ORDER,
};
use crate::buddy::launchers::{
    command_kind_for, is_wecode_claude_command, is_wecode_codex_command, split_command,
    LauncherCommandKind,
};
use crate::buddy::parsers::parse_jsonl_buffer;
use crate::buddy::paths::{canonical_repo_root, create_buddy_paths, task_dir, workspace_key_for_repo};
use crate::buddy::redact::redact_json_value;
use crate::buddy::schemas::{
    parse_event_line, parse_global_settings, parse_task_settings, parse_task_state, SchemaError,
};
use crate::buddy::types::{
    AttachmentMeta, CreateTaskInput, CreateTaskResult, Event, ExecutionMode, GlobalSettings,
    InstructionQueueItem, Launcher, RoundEventEntry, RoundEventSummary, Task, TaskActorStats,
    TaskDetail, TaskQueueInfo, TaskSettings, TaskState, TaskStats, TaskStatus, TranscriptEntry,
};
use chrono::{DateTime, Utc};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tokio::fs;
use tokio::io::AsyncWriteExt;

const ACTORS: [&str; 5] = ["claude", "codex", "cursor", "opencode", "kimi"];

const TRANSCRIPT_ROLES: [&str; 7] = [
    "human", "claude", "codex", "cursor", "opencode", "kimi", "system",
];

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid data: {0}")]
    Invalid(String),
}

impl From<SchemaError> for StoreError {
    fn from(error: SchemaError) -> Self {
        StoreError::Invalid(error.0)
    }
}

impl From<serde_json::Error> for StoreError {
    fn from(error: serde_json::Error) -> Self {
        StoreError::Invalid(error.to_string())
    }
}

/// Input for [`BuddyStore::append_task_event`], mirroring the Electron
/// edition's `Omit<Event, 'seq' | 'ts'> & Partial<Pick<Event, 'seq' | 'ts'>>`.
#[derive(Debug, Clone, Default)]
pub struct EventInput {
    pub event_type: String,
    pub actor: Option<String>,
    pub run_id: Option<String>,
    pub payload: Map<String, Value>,
    pub seq: Option<u64>,
    pub ts: Option<String>,
}

pub struct BuddyStore {
    pub data_root: PathBuf,
    /// Override for the user home directory used by session-insight lookups
    /// (kimi `wire.jsonl`, opencode storage, CLI config files). `None` uses
    /// the real home directory; tests pass a temp dir for hermetic runs.
    home_dir: Option<PathBuf>,
}

impl BuddyStore {
    pub fn new(data_root: impl Into<PathBuf>) -> Self {
        BuddyStore {
            data_root: data_root.into(),
            home_dir: None,
        }
    }

    pub fn with_home_dir(mut self, home_dir: impl Into<PathBuf>) -> Self {
        self.home_dir = Some(home_dir.into());
        self
    }

    fn home(&self) -> PathBuf {
        self.home_dir
            .clone()
            .or_else(dirs::home_dir)
            .unwrap_or_default()
    }

    pub async fn get_tasks(&self) -> Vec<Task> {
        let paths = create_buddy_paths(&self.data_root);
        let workspace_keys = list_directory_names(&paths.workspaces_dir).await;
        let mut tasks: Vec<Task> = Vec::new();

        for workspace_key in workspace_keys {
            let tasks_dir = paths.workspaces_dir.join(&workspace_key).join("tasks");
            let task_ids = list_directory_names(&tasks_dir).await;
            for task_id in task_ids {
                // Ignore unreadable task directories; schema errors surface on
                // detail load (same as the Electron edition).
                if let Ok(state) = self.read_task_state(&task_id, &workspace_key).await {
                    tasks.push(Task {
                        task_id,
                        workspace_key: workspace_key.clone(),
                        status: state.status.clone(),
                        updated_at: state.updated_at.clone().unwrap_or_default(),
                        repo_root: state.repo_root.clone().unwrap_or_default(),
                        round: Some(state.round),
                        active_run: state.active_run.clone(),
                        execution_mode: Some(
                            state.execution_mode.unwrap_or(ExecutionMode::Immediate),
                        ),
                        queue: state.queue.clone(),
                        created_at: state.created_at.clone(),
                    });
                }
            }
        }

        tasks.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        tasks
    }

    pub async fn get_task_detail(
        &self,
        task_id: &str,
        workspace_key: &str,
    ) -> Result<TaskDetail, StoreError> {
        let state = self.read_task_state(task_id, workspace_key).await?;
        let settings = self.read_task_settings(task_id, workspace_key).await?;
        let meta = self.read_task_meta(task_id, workspace_key).await;
        let events = self.read_events(task_id, workspace_key).await;
        let transcript = self.read_transcript_jsonl(task_id, workspace_key).await;
        let latest_failure = state
            .latest_failure
            .clone()
            .or_else(|| state.last_error.clone());

        Ok(TaskDetail {
            task_id: task_id.to_string(),
            workspace_key: workspace_key.to_string(),
            state,
            settings,
            task_text: meta.task_text.unwrap_or_default(),
            context_text: meta.context_text.unwrap_or_default(),
            transcript,
            events,
            latest_failure,
        })
    }

    pub async fn get_events(&self, task_id: &str, since: u64, workspace_key: &str) -> Vec<Event> {
        self.read_events(task_id, workspace_key)
            .await
            .into_iter()
            .filter(|event| event.seq > since)
            .collect()
    }

    pub async fn create_task(&self, input: CreateTaskInput) -> Result<CreateTaskResult, StoreError> {
        let repo_root = canonical_repo_root(input.repo_root.as_deref().unwrap_or(""));
        let repo_root = repo_root.to_string_lossy().to_string();
        let workspace_key = workspace_key_for_repo(if repo_root.is_empty() {
            &input.task_id
        } else {
            &repo_root
        });
        let task_id = self.deduplicate_task_id(&input.task_id, &workspace_key).await?;
        let dir = self.task_directory(&task_id, &workspace_key);
        let now = utc_now();
        let task_text = task_markdown_content(input.task_text.as_deref().unwrap_or(""));
        let context_text = context_markdown_content(input.context_text.as_deref().unwrap_or(""));

        let global_settings = self.read_global_settings().await?;
        let settings_value = default_task_settings(&global_settings, input.settings.as_ref());
        let settings: TaskSettings = parse_task_settings(&settings_value)?;
        let execution_mode = input.execution_mode.unwrap_or(ExecutionMode::Immediate);
        let mut state = default_task_state(&task_id, &repo_root, &settings, &context_text, &now);
        match execution_mode {
            ExecutionMode::Queued => {
                state.status = TaskStatus::Queued;
                state.execution_mode = Some(ExecutionMode::Queued);
                state.queue = Some(TaskQueueInfo {
                    state: "waiting".to_string(),
                    enqueued_at: now.clone(),
                    activated_at: None,
                    activation_source: None,
                });
            }
            ExecutionMode::Immediate => {
                state.execution_mode = Some(ExecutionMode::Immediate);
            }
        }
        state.event_seq = Some(1);

        fs::create_dir_all(dir.join("rounds")).await?;
        fs::create_dir_all(dir.join("artifacts")).await?;
        self.write_workspace_metadata(&workspace_key, &repo_root, &now)
            .await?;
        atomic_write_text(&dir.join("task.md"), &task_text).await?;
        atomic_write_text(&dir.join("context.md"), &context_text).await?;
        atomic_write_json(&dir.join("settings.json"), &settings_value).await?;
        atomic_write_json(&dir.join("state.json"), &state_to_json_value(&state)).await?;
        atomic_write_text(&dir.join("status"), &format!("{}\n", state.status)).await?;
        atomic_append_text(&dir.join(".buddy.lock"), "").await?;

        let mut payload = Map::new();
        payload.insert("task_id".to_string(), Value::String(task_id.clone()));
        payload.insert(
            "execution_mode".to_string(),
            serde_json::to_value(execution_mode)?,
        );
        let created = Event {
            seq: 1,
            task_id: Some(task_id.clone()),
            event_type: "task.created".to_string(),
            actor: None,
            ts: now.clone(),
            run_id: None,
            payload,
        };
        append_event_line(
            &dir.join("events.jsonl"),
            &serde_json::to_value(&created)?,
        )
        .await?;
        if execution_mode == ExecutionMode::Queued {
            let mut queued_payload = Map::new();
            queued_payload.insert(
                "workspace_key".to_string(),
                Value::String(workspace_key.clone()),
            );
            queued_payload.insert("task_id".to_string(), Value::String(task_id.clone()));
            queued_payload.insert("enqueued_at".to_string(), Value::String(now.clone()));
            let queued = Event {
                seq: 2,
                task_id: Some(task_id.clone()),
                event_type: "task.queued".to_string(),
                actor: None,
                ts: now.clone(),
                run_id: None,
                payload: queued_payload,
            };
            append_event_line(
                &dir.join("events.jsonl"),
                &serde_json::to_value(&queued)?,
            )
            .await?;
        }

        Ok(CreateTaskResult {
            task: task_id,
            path: dir.to_string_lossy().to_string(),
            workspace_key,
        })
    }

    pub async fn delete_task(&self, task_id: &str, workspace_key: &str) -> Result<(), StoreError> {
        let dir = self.task_directory(task_id, workspace_key);
        match fs::remove_dir_all(&dir).await {
            Ok(()) => Ok(()),
            // `rm -rf` semantics: missing directories are not an error.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    pub async fn update_global_settings(
        &self,
        settings: &GlobalSettings,
    ) -> Result<GlobalSettings, StoreError> {
        let path = create_buddy_paths(&self.data_root).global_settings;
        let normalized = normalize_global_settings(Some(settings));
        atomic_write_json(&path, &serde_json::to_value(&normalized)?).await?;
        Ok(normalized)
    }

    pub async fn read_global_settings(&self) -> Result<GlobalSettings, StoreError> {
        let path = create_buddy_paths(&self.data_root).global_settings;
        let legacy_path = self.data_root.join("global_settings.json");
        match read_json(&path).await {
            Ok(value) => {
                let parsed = parse_global_settings(&value)?;
                return Ok(normalize_global_settings(Some(&parsed)));
            }
            Err(ReadJsonError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(ReadJsonError::Io(error)) => return Err(error.into()),
            Err(ReadJsonError::Parse(error)) => return Err(error.into()),
        }

        match read_json(&legacy_path).await {
            Ok(value) => {
                let parsed = parse_global_settings(&value)?;
                Ok(normalize_global_settings(Some(&parsed)))
            }
            Err(ReadJsonError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(normalize_global_settings(None))
            }
            Err(ReadJsonError::Io(error)) => Err(error.into()),
            Err(ReadJsonError::Parse(error)) => Err(error.into()),
        }
    }

    pub async fn read_task_state(
        &self,
        task_id: &str,
        workspace_key: &str,
    ) -> Result<TaskState, StoreError> {
        let value = read_json(&self.state_path(task_id, workspace_key)).await?;
        Ok(parse_task_state(&value)?)
    }

    pub async fn read_task_settings(
        &self,
        task_id: &str,
        workspace_key: &str,
    ) -> Result<TaskSettings, StoreError> {
        let value = read_json(&self.settings_path(task_id, workspace_key)).await?;
        Ok(parse_task_settings(&value)?)
    }

    pub async fn update_task_state<F>(
        &self,
        task_id: &str,
        workspace_key: &str,
        update: F,
    ) -> Result<TaskState, StoreError>
    where
        F: FnOnce(TaskState) -> TaskState,
    {
        let next = update(self.read_task_state(task_id, workspace_key).await?);
        self.write_task_state(task_id, workspace_key, next).await
    }

    pub async fn append_task_event(
        &self,
        task_id: &str,
        workspace_key: &str,
        input: EventInput,
    ) -> Result<Event, StoreError> {
        let events = self.read_events(task_id, workspace_key).await;
        let state = self.read_task_state(task_id, workspace_key).await?;
        let max_logged_seq = events.iter().map(|event| event.seq).max().unwrap_or(0);
        let seq = input.seq.unwrap_or_else(|| {
            std::cmp::max(state.event_seq.unwrap_or(0), max_logged_seq) + 1
        });
        let next = Event {
            seq,
            task_id: Some(task_id.to_string()),
            event_type: input.event_type,
            actor: input.actor,
            ts: input.ts.unwrap_or_else(utc_now),
            run_id: input.run_id,
            payload: input.payload,
        };
        let redacted = redact_json_value(&serde_json::to_value(&next)?);
        append_event_line(&self.events_path(task_id, workspace_key), &redacted).await?;
        let mut new_state = state;
        new_state.event_seq = Some(seq);
        self.write_task_state(task_id, workspace_key, new_state).await?;
        Ok(serde_json::from_value(redacted)?)
    }

    pub async fn append_transcript(
        &self,
        task_id: &str,
        workspace_key: &str,
        role: &str,
        content: &str,
        meta: Map<String, Value>,
    ) -> Result<TranscriptEntry, StoreError> {
        let state = self.read_task_state(task_id, workspace_key).await?;
        let seq = state.transcript_seq.unwrap_or(0) + 1;
        let ts = utc_now();
        let mut row = Map::new();
        row.insert("seq".to_string(), Value::from(seq));
        row.insert("ts".to_string(), Value::String(ts.clone()));
        row.insert("role".to_string(), Value::String(role.to_string()));
        row.insert("content".to_string(), Value::String(content.to_string()));
        row.insert("meta".to_string(), Value::Object(meta.clone()));
        atomic_append_text(
            &self.transcript_jsonl_path(task_id, workspace_key),
            &format!("{}\n", stringify_python_json_line(&Value::Object(row))),
        )
        .await?;
        let mut new_state = state;
        new_state.transcript_seq = Some(seq);
        self.write_task_state(task_id, workspace_key, new_state).await?;
        Ok(TranscriptEntry {
            role: role.to_string(),
            content: content.to_string(),
            ts,
            round: None,
            meta: Some(meta),
            seq: Some(seq),
        })
    }

    pub async fn enqueue_instruction(
        &self,
        task_id: &str,
        workspace_key: &str,
        content: &str,
        attachments: Option<Vec<AttachmentMeta>>,
    ) -> Result<InstructionQueueItem, StoreError> {
        let item = InstructionQueueItem {
            id: new_queue_item_id(),
            content: content.to_string(),
            created_at: utc_now(),
            attachments: attachments.filter(|items| !items.is_empty()),
        };
        let queued = item.clone();
        self.update_task_state(task_id, workspace_key, move |mut state| {
            state
                .instruction_queue
                .get_or_insert_with(Vec::new)
                .push(queued);
            state
        })
        .await?;
        Ok(item)
    }

    pub async fn dequeue_instruction(
        &self,
        task_id: &str,
        workspace_key: &str,
        item_id: &str,
    ) -> Result<(), StoreError> {
        let item_id = item_id.to_string();
        self.update_task_state(task_id, workspace_key, move |mut state| {
            let queue = state.instruction_queue.take().unwrap_or_default();
            state.instruction_queue = Some(
                queue
                    .into_iter()
                    .filter(|item| item.id != item_id)
                    .collect(),
            );
            state
        })
        .await?;
        Ok(())
    }

    pub async fn clear_instruction_queue(
        &self,
        task_id: &str,
        workspace_key: &str,
    ) -> Result<(), StoreError> {
        self.update_task_state(task_id, workspace_key, |mut state| {
            state.instruction_queue = Some(Vec::new());
            state
        })
        .await?;
        Ok(())
    }

    pub async fn drain_instruction_queue(
        &self,
        task_id: &str,
        workspace_key: &str,
    ) -> Result<Vec<InstructionQueueItem>, StoreError> {
        let state = self.read_task_state(task_id, workspace_key).await?;
        let items = state.instruction_queue.clone().unwrap_or_default();
        if items.is_empty() {
            return Ok(items);
        }
        let mut new_state = state;
        new_state.instruction_queue = Some(Vec::new());
        self.write_task_state(task_id, workspace_key, new_state).await?;
        Ok(items)
    }

    pub fn task_directory(&self, task_id: &str, workspace_key: &str) -> PathBuf {
        task_dir(
            &create_buddy_paths(&self.data_root),
            workspace_key,
            task_id,
        )
    }

    pub fn state_path(&self, task_id: &str, workspace_key: &str) -> PathBuf {
        self.task_directory(task_id, workspace_key).join("state.json")
    }

    pub fn settings_path(&self, task_id: &str, workspace_key: &str) -> PathBuf {
        self.task_directory(task_id, workspace_key)
            .join("settings.json")
    }

    pub fn events_path(&self, task_id: &str, workspace_key: &str) -> PathBuf {
        self.task_directory(task_id, workspace_key)
            .join("events.jsonl")
    }

    pub fn transcript_jsonl_path(&self, task_id: &str, workspace_key: &str) -> PathBuf {
        self.task_directory(task_id, workspace_key)
            .join("transcript.jsonl")
    }

    pub async fn update_task_text(
        &self,
        task_id: &str,
        workspace_key: &str,
        task_text: &str,
    ) -> Result<(), StoreError> {
        let dir = self.task_directory(task_id, workspace_key);
        atomic_write_text(&dir.join("task.md"), &task_markdown_content(task_text)).await
    }

    pub async fn read_transcript_jsonl(
        &self,
        task_id: &str,
        workspace_key: &str,
    ) -> Vec<TranscriptEntry> {
        let text = read_optional_text(&self.transcript_jsonl_path(task_id, workspace_key)).await;
        if text.trim().is_empty() {
            return Vec::new();
        }

        text.split('\n')
            .map(|line| line.strip_suffix('\r').unwrap_or(line))
            .filter(|line| !line.trim().is_empty())
            .filter_map(|line| {
                serde_json::from_str::<Value>(line)
                    .ok()
                    .and_then(|value| transcript_entry_from_json(&value))
            })
            .collect()
    }

    /// Writes `state.json` (+ the `status` sidecar), forcing
    /// `protocol_version` and monotonic `event_seq` / `transcript_seq`,
    /// exactly like the Electron edition's private `writeTaskState`.
    pub async fn write_task_state(
        &self,
        task_id: &str,
        workspace_key: &str,
        state: TaskState,
    ) -> Result<TaskState, StoreError> {
        let current = read_optional_json(&self.state_path(task_id, workspace_key)).await;
        let mut next = state;
        next.protocol_version = Some("1".to_string());
        next.event_seq = Some(std::cmp::max(
            next.event_seq.unwrap_or(0),
            json_number(current.get("event_seq")).unwrap_or(0.0) as u64,
        ));
        next.transcript_seq = Some(std::cmp::max(
            next.transcript_seq.unwrap_or(0),
            json_number(current.get("transcript_seq")).unwrap_or(0.0) as u64,
        ));
        next.updated_at = Some(utc_now());
        atomic_write_json(
            &self.state_path(task_id, workspace_key),
            &state_to_json_value(&next),
        )
        .await?;
        atomic_write_text(
            &self.task_directory(task_id, workspace_key).join("status"),
            &format!("{}\n", next.status),
        )
        .await?;
        Ok(next)
    }

    async fn write_workspace_metadata(
        &self,
        workspace_key: &str,
        repo_root: &str,
        now: &str,
    ) -> Result<(), StoreError> {
        let mut value = Map::new();
        value.insert("protocol_version".to_string(), Value::String("1".to_string()));
        value.insert(
            "workspace_key".to_string(),
            Value::String(workspace_key.to_string()),
        );
        value.insert(
            "default_repo_root".to_string(),
            Value::String(repo_root.to_string()),
        );
        value.insert("updated_at".to_string(), Value::String(now.to_string()));
        atomic_write_json(
            &create_buddy_paths(&self.data_root)
                .workspaces_dir
                .join(workspace_key)
                .join("workspace.json"),
            &Value::Object(value),
        )
        .await
    }

    async fn read_task_meta(&self, task_id: &str, workspace_key: &str) -> TaskMeta {
        if let Some(markdown) = self.read_markdown_task_meta(task_id, workspace_key).await {
            return markdown;
        }

        let path = self.task_directory(task_id, workspace_key).join("task.json");
        match read_json(&path).await {
            Ok(value) => TaskMeta {
                task_text: value
                    .get("task_text")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                context_text: value
                    .get("context_text")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            },
            Err(_) => TaskMeta {
                task_text: Some(String::new()),
                context_text: Some(String::new()),
            },
        }
    }

    async fn read_markdown_task_meta(&self, task_id: &str, workspace_key: &str) -> Option<TaskMeta> {
        let dir = self.task_directory(task_id, workspace_key);
        let task_text = read_optional_text(&dir.join("task.md")).await;
        let context_text = read_optional_text(&dir.join("context.md")).await;
        if task_text.is_empty() && context_text.is_empty() {
            return None;
        }
        Some(TaskMeta {
            task_text: Some(task_text),
            context_text: Some(context_text),
        })
    }

    async fn read_events(&self, task_id: &str, workspace_key: &str) -> Vec<Event> {
        let path = self.events_path(task_id, workspace_key);
        let text = match fs::read_to_string(&path).await {
            Ok(text) => text,
            Err(_) => return Vec::new(),
        };
        // One malformed line invalidates the whole log (the Electron edition
        // wraps the entire map(parseEventLine) in a single try/catch).
        let mut events = Vec::new();
        for line in text
            .split('\n')
            .map(|line| line.strip_suffix('\r').unwrap_or(line))
            .filter(|line| !line.is_empty())
        {
            match parse_event_line(line) {
                Ok(event) => events.push(event),
                Err(_) => return Vec::new(),
            }
        }
        events
    }

    async fn deduplicate_task_id(
        &self,
        base_id: &str,
        workspace_key: &str,
    ) -> Result<String, StoreError> {
        if !directory_exists(&self.task_directory(base_id, workspace_key)).await {
            return Ok(base_id.to_string());
        }
        for i in 2..=999 {
            let candidate = format!("{base_id}_{i}");
            if !directory_exists(&self.task_directory(&candidate, workspace_key)).await {
                return Ok(candidate);
            }
        }
        Err(StoreError::Invalid(format!(
            "Cannot deduplicate task ID: {base_id}"
        )))
    }

    /// Session id an actor used for this task, from persisted task state.
    async fn actor_session_id(
        &self,
        task_id: &str,
        workspace_key: &str,
        actor: &str,
    ) -> Option<String> {
        let state = self.read_task_state(task_id, workspace_key).await.ok()?;
        let value = match actor {
            "kimi" => state.kimi_session_id,
            "opencode" => state.opencode_session_id,
            "claude" => state.claude_session_id,
            "codex" => state.codex_thread_id,
            _ => None,
        };
        value.filter(|session_id| !session_id.is_empty())
    }
}

struct TaskMeta {
    task_text: Option<String>,
    context_text: Option<String>,
}

// ---------------------------------------------------------------------------
// Round events & task stats (getRoundEvents / getTaskStats)
// ---------------------------------------------------------------------------

impl BuddyStore {
    pub async fn get_round_events(
        &self,
        task_id: &str,
        run_id: &str,
        workspace_key: &str,
        actor: Option<&str>,
        command: Option<&str>,
    ) -> Option<RoundEventSummary> {
        let dir = self.task_directory(task_id, workspace_key);
        let events_path = dir.join("artifacts").join(format!("{run_id}-events.jsonl"));
        let raw = read_optional_text(&events_path).await;
        if raw.trim().is_empty() {
            return None;
        }

        let mut events: Vec<RoundEventEntry> = Vec::new();
        let mut input_tokens: u64 = 0;
        let mut output_tokens: u64 = 0;
        let mut cache_read_tokens: u64 = 0;
        let mut duration_ms: Option<f64> = None;
        let mut cost_usd: Option<f64> = None;
        let mut model: Option<String> = None;
        let mut first_ts: Option<f64> = None;
        let mut last_ts: Option<f64> = None;

        for event in parse_jsonl_buffer(&raw) {
            // Track timestamps for duration calculation
            let ts = event
                .get("timestamp")
                .filter(|value| !value.is_null())
                .or_else(|| event.get("ts"));
            let ts_ms = match ts {
                Some(Value::Number(_)) => ts.and_then(Value::as_f64),
                Some(Value::String(text)) => parse_ts_ms(text),
                _ => None,
            };
            if let Some(ms) = ts_ms {
                if first_ts.map_or(true, |first| ms < first) {
                    first_ts = Some(ms);
                }
                if last_ts.map_or(true, |last| ms > last) {
                    last_ts = Some(ms);
                }
            }

            let event_type = event.get("type").and_then(Value::as_str);

            // Claude stream-json format
            if event_type == Some("system")
                && event.get("subtype").and_then(Value::as_str) == Some("init")
            {
                model = event
                    .get("model")
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }

            if event_type == Some("assistant") {
                if let Some(content) = event
                    .get("message")
                    .and_then(|message| message.get("content"))
                    .and_then(Value::as_array)
                {
                    for part in content {
                        match part.get("type").and_then(Value::as_str) {
                            Some("thinking") => {
                                let thinking_length = part
                                    .get("thinking")
                                    .and_then(Value::as_str)
                                    .map(|thinking| thinking.chars().count() as u64)
                                    .unwrap_or(0);
                                events.push(thinking_entry(thinking_length));
                            }
                            Some("text") => {
                                events.push(text_entry(
                                    part.get("text")
                                        .and_then(Value::as_str)
                                        .map(str::to_string),
                                ));
                            }
                            Some("tool_use") => {
                                events.push(tool_use_entry(
                                    part.get("name")
                                        .and_then(Value::as_str)
                                        .map(str::to_string),
                                    part.get("input").and_then(Value::as_object).cloned(),
                                ));
                            }
                            _ => {}
                        }
                    }
                }
            }

            if event_type == Some("user") {
                if let Some(content) = event
                    .get("message")
                    .and_then(|message| message.get("content"))
                    .and_then(Value::as_array)
                {
                    for part in content {
                        if part.get("type").and_then(Value::as_str) == Some("tool_result") {
                            let preview = match part.get("content") {
                                Some(Value::String(text)) => take_chars(text, 200),
                                Some(other) => {
                                    take_chars(&serde_json::to_string(other).unwrap_or_default(), 200)
                                }
                                None => String::new(),
                            };
                            events.push(tool_result_entry(
                                preview,
                                part.get("is_error").and_then(Value::as_bool),
                            ));
                        }
                    }
                }
            }

            if event_type == Some("result") {
                if let Some(usage) = event.get("usage").and_then(Value::as_object) {
                    input_tokens = json_u64(usage.get("input_tokens")).unwrap_or(input_tokens);
                    output_tokens = json_u64(usage.get("output_tokens")).unwrap_or(output_tokens);
                    cache_read_tokens =
                        json_u64(usage.get("cache_read_input_tokens")).unwrap_or(cache_read_tokens);
                }
                if let Some(value) = event.get("duration_ms").filter(|v| !v.is_null()) {
                    duration_ms = value.as_f64().or(duration_ms);
                }
                if let Some(value) = event.get("total_cost_usd").filter(|v| !v.is_null()) {
                    cost_usd = value.as_f64().or(cost_usd);
                }
                if let Some(value) = event.get("model").and_then(Value::as_str) {
                    model = Some(value.to_string());
                }
                // Claude's result event has modelUsage with model name as key
                // e.g. modelUsage: { "thudm-glm-5.1": { inputTokens: ..., ... } }
                if model.is_none() {
                    if let Some(model_usage) = event.get("modelUsage").and_then(Value::as_object) {
                        if let Some(first_key) = model_usage.keys().next() {
                            model = Some(first_key.clone());
                        }
                    }
                }
            }

            // Codex format: content array with tool_call / text
            if let Some(content) = event.get("content").and_then(Value::as_array) {
                for part in content {
                    let part_type = part.get("type").and_then(Value::as_str);
                    if part_type == Some("tool_call") {
                        if let Some(name) = part.get("name").and_then(Value::as_str) {
                            events.push(tool_use_entry(
                                Some(name.to_string()),
                                part.get("input").and_then(Value::as_object).cloned(),
                            ));
                        }
                    } else if part_type == Some("text") {
                        if let Some(text) = part.get("text").and_then(Value::as_str) {
                            events.push(text_entry(Some(text.to_string())));
                        }
                    }
                }
            }
            if event_type == Some("response.completed") {
                if let Some(response) = event.get("response").and_then(Value::as_object) {
                    if let Some(usage) = response.get("usage").and_then(Value::as_object) {
                        input_tokens = json_u64(usage.get("input_tokens")).unwrap_or(input_tokens);
                        output_tokens =
                            json_u64(usage.get("output_tokens")).unwrap_or(output_tokens);
                    }
                    if let Some(value) = response.get("model").and_then(Value::as_str) {
                        model = Some(value.to_string());
                    }
                }
            }

            // Kimi format: role=assistant with content
            if event.get("role").and_then(Value::as_str) == Some("assistant") {
                if let Some(content) = event.get("content").and_then(Value::as_str) {
                    if !content.trim().is_empty() {
                        events.push(text_entry(Some(content.to_string())));
                    }
                }
            }
            // Kimi tool calls
            if let Some(tool_calls) = event.get("tool_calls").and_then(Value::as_array) {
                for tool_call in tool_calls {
                    let tool_name = tool_call
                        .get("function")
                        .and_then(Value::as_str)
                        .or_else(|| tool_call.get("name").and_then(Value::as_str))
                        .map(str::to_string);
                    events.push(tool_use_entry(
                        tool_name,
                        tool_call
                            .get("arguments")
                            .and_then(Value::as_object)
                            .cloned(),
                    ));
                }
            }
            // Kimi/OpenAI-compatible: usage and model in final response
            if let Some(usage) = event.get("usage").and_then(Value::as_object) {
                if usage.get("input_tokens").is_some_and(|v| !v.is_null()) {
                    input_tokens = json_u64(usage.get("input_tokens")).unwrap_or(input_tokens);
                }
                if usage.get("prompt_tokens").is_some_and(|v| !v.is_null()) {
                    input_tokens = json_u64(usage.get("prompt_tokens")).unwrap_or(input_tokens);
                }
                if usage.get("output_tokens").is_some_and(|v| !v.is_null()) {
                    output_tokens = json_u64(usage.get("output_tokens")).unwrap_or(output_tokens);
                }
                if usage.get("completion_tokens").is_some_and(|v| !v.is_null()) {
                    output_tokens =
                        json_u64(usage.get("completion_tokens")).unwrap_or(output_tokens);
                }
            }
            if model.is_none() {
                if let Some(value) = event
                    .get("model")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                {
                    model = Some(value.to_string());
                }
            }

            // OpenCode format: tool_use events with part.tool, input in part.state.input
            if event_type == Some("tool_use") {
                let part = event.get("part").and_then(Value::as_object);
                let tool_name = part
                    .and_then(|part| part.get("tool"))
                    .and_then(Value::as_str)
                    .unwrap_or("tool")
                    .to_string();
                // OpenCode stores tool input in part.state.input, not part.input
                let state = part.and_then(|part| part.get("state")).and_then(Value::as_object);
                let tool_input = state
                    .and_then(|state| state.get("input"))
                    .and_then(Value::as_object)
                    .cloned()
                    .or_else(|| {
                        part.and_then(|part| part.get("input"))
                            .and_then(Value::as_object)
                            .cloned()
                    });
                events.push(tool_use_entry(Some(tool_name), tool_input));
                // OpenCode tool result is in part.state.output
                if let Some(output) = state.and_then(|state| state.get("output")) {
                    if js_truthy(output) {
                        let preview = match output {
                            Value::String(text) => take_chars(text, 200),
                            other => take_chars(&serde_json::to_string(other).unwrap_or_default(), 200),
                        };
                        let exit = state
                            .and_then(|state| state.get("metadata"))
                            .and_then(|metadata| metadata.get("exit"));
                        let exit_non_zero = matches!(exit, Some(value) if !value.is_null() && value.as_f64() != Some(0.0));
                        let is_error = state
                            .and_then(|state| state.get("status"))
                            .and_then(Value::as_str)
                            == Some("error")
                            || exit_non_zero;
                        events.push(tool_result_entry(preview, Some(is_error)));
                    }
                }
            }
            if event_type == Some("text") {
                let part = event.get("part").and_then(Value::as_object);
                if let Some(text) = part
                    .and_then(|part| part.get("text"))
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                {
                    events.push(text_entry(Some(text.to_string())));
                }
            }
            // OpenCode step_finish: tokens in part.tokens, cost in part.cost,
            // model in part.respondedModelID
            if event_type == Some("step_finish") {
                let part = event.get("part").and_then(Value::as_object);
                if let Some(tokens) = part
                    .and_then(|part| part.get("tokens"))
                    .and_then(Value::as_object)
                {
                    let cache_read = tokens
                        .get("cache")
                        .and_then(|cache| cache.get("read"))
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    input_tokens = json_u64(tokens.get("input")).unwrap_or(0);
                    cache_read_tokens = cache_read;
                    output_tokens = json_u64(tokens.get("output")).unwrap_or(output_tokens);
                }
                if let Some(cost) = part
                    .and_then(|part| part.get("cost"))
                    .filter(|value| !value.is_null())
                {
                    cost_usd = cost.as_f64().or(cost_usd);
                }
                let responded = part
                    .and_then(|part| part.get("respondedModelID"))
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty());
                let requested = part
                    .and_then(|part| part.get("requestedModelID"))
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty());
                if let Some(responded) = responded {
                    model = Some(responded.to_string());
                } else if let Some(requested) = requested {
                    model = Some(requested.to_string());
                }
            }

            // Generic: item.text
            if let Some(item) = event.get("item").and_then(Value::as_object) {
                if let Some(text) = item
                    .get("text")
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                {
                    events.push(text_entry(Some(text.to_string())));
                }
            }
        }

        // Fallback: compute duration from event timestamps if not provided by actor
        if duration_ms.is_none() {
            if let (Some(first), Some(last)) = (first_ts, last_ts) {
                if last > first {
                    duration_ms = Some(last - first);
                }
            }
        }

        // Fallback: detect model from actor session state / config file when
        // not available in streaming output
        if model.is_none() {
            if let Some(actor) = actor {
                let mut command = command.map(str::to_string);
                // If command wasn't provided by caller, try reading it from task settings
                if command.is_none() {
                    if let Ok(settings) = self.read_task_settings(task_id, workspace_key).await {
                        if let Some(launcher) = settings.launchers.get(actor) {
                            if !launcher.command.is_empty() {
                                command = Some(launcher.command.clone());
                            }
                        }
                    }
                }
                // kimi/opencode stdout carries no model — read it from their session state
                let session_id = self.actor_session_id(task_id, workspace_key, actor).await;
                if actor == "kimi" {
                    if let Some(session_id) = &session_id {
                        if let Some(insight) =
                            read_kimi_session_insight(&self.home(), session_id).await
                        {
                            if insight.model.is_some() {
                                model = insight.model;
                            }
                        }
                    }
                } else if actor == "opencode" {
                    if let Some(session_id) = &session_id {
                        model = read_opencode_session_model(&self.home(), session_id).await;
                    }
                }
                if model.is_none() {
                    model = detect_model_from_config(&self.home(), actor, command.as_deref()).await;
                }
            }
        }

        Some(RoundEventSummary {
            run_id: run_id.to_string(),
            events,
            input_tokens,
            output_tokens,
            cache_read_tokens,
            duration_ms: duration_ms.map(|value| value as u64),
            cost_usd,
            model,
        })
    }

    pub async fn get_task_stats(&self, task_id: &str, workspace_key: &str) -> Option<TaskStats> {
        let transcript = self.read_transcript_jsonl(task_id, workspace_key).await;
        if transcript.is_empty() {
            return None;
        }

        // Collect run_ids grouped by actor (insertion order, like a JS Map),
        // and track elapsed_ms per run
        let mut actor_runs: Vec<(String, Vec<RunInfo>)> = Vec::new();
        for entry in &transcript {
            if !ACTORS.contains(&entry.role.as_str()) {
                continue;
            }
            let meta = entry.meta.as_ref();
            let run_id = meta
                .and_then(|meta| meta.get("run_id"))
                .and_then(Value::as_str);
            let Some(run_id) = run_id else { continue };
            let elapsed_ms = meta
                .and_then(|meta| meta.get("elapsed_ms"))
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            let end_ts = parse_ts_ms(&entry.ts);
            let run = RunInfo {
                run_id: run_id.to_string(),
                elapsed_ms,
                end_ts,
            };
            if let Some((_, runs)) = actor_runs.iter_mut().find(|(actor, _)| actor == &entry.role) {
                runs.push(run);
            } else {
                actor_runs.push((entry.role.clone(), vec![run]));
            }
        }

        if actor_runs.is_empty() {
            return None;
        }

        let mut actors: Vec<TaskActorStats> = Vec::new();

        for (actor, runs) in &actor_runs {
            let mut input_tokens: u64 = 0;
            let mut output_tokens: u64 = 0;
            let mut cache_read_tokens: u64 = 0;
            let mut duration_ms: f64 = 0.0;
            let mut cost_usd: Option<f64> = None;
            let mut model: Option<String> = None;

            for run in runs {
                duration_ms += run.elapsed_ms;
                if let Some(summary) = self
                    .get_round_events(task_id, &run.run_id, workspace_key, Some(actor), None)
                    .await
                {
                    input_tokens += summary.input_tokens;
                    output_tokens += summary.output_tokens;
                    cache_read_tokens += summary.cache_read_tokens;
                    if let Some(summary_duration) = summary.duration_ms {
                        if summary_duration > 0 {
                            // Use actor-reported duration if available,
                            // otherwise fall back to elapsed_ms
                            duration_ms = duration_ms - run.elapsed_ms + summary_duration as f64;
                        }
                    }
                    if let Some(summary_cost) = summary.cost_usd {
                        cost_usd = Some(cost_usd.unwrap_or(0.0) + summary_cost);
                    }
                    // Use the latest model reported by the actor
                    if summary.model.is_some() {
                        model = summary.model;
                    }
                }
            }

            // kimi stdout carries no usage events — attribute wire.jsonl usage
            // records to runs by their [endTs - elapsedMs, endTs] time windows.
            if actor == "kimi" && input_tokens + output_tokens + cache_read_tokens == 0 {
                let session_id = self.actor_session_id(task_id, workspace_key, actor).await;
                let insight = match session_id {
                    Some(session_id) => read_kimi_session_insight(&self.home(), &session_id).await,
                    None => None,
                };
                if let Some(insight) = insight {
                    const WINDOW_SLACK_MS: f64 = 5_000.0;
                    for record in &insight.records {
                        let matched = runs.iter().any(|run| {
                            run.end_ts.is_some_and(|end_ts| {
                                record.time_ms >= end_ts - run.elapsed_ms - WINDOW_SLACK_MS
                                    && record.time_ms <= end_ts + WINDOW_SLACK_MS
                            })
                        });
                        if !matched {
                            continue;
                        }
                        input_tokens += record.input_tokens;
                        output_tokens += record.output_tokens;
                        cache_read_tokens += record.cache_read_tokens;
                    }
                    if model.is_none() && insight.model.is_some() {
                        model = insight.model;
                    }
                }
            }

            actors.push(TaskActorStats {
                actor: actor.clone(),
                model,
                input_tokens,
                output_tokens,
                cache_read_tokens,
                duration_ms: duration_ms as u64,
                cost_usd,
                rounds: runs.len() as u32,
            });
        }

        let total_input_tokens = actors.iter().map(|a| a.input_tokens).sum();
        let total_output_tokens = actors.iter().map(|a| a.output_tokens).sum();
        let total_cache_read_tokens = actors.iter().map(|a| a.cache_read_tokens).sum();
        let total_duration_ms = actors.iter().map(|a| a.duration_ms).sum();
        let total_rounds = actors.iter().map(|a| a.rounds).sum();
        let has_cost = actors.iter().any(|a| a.cost_usd.is_some());
        let total_cost_usd = if has_cost {
            Some(actors.iter().map(|a| a.cost_usd.unwrap_or(0.0)).sum())
        } else {
            None
        };

        Some(TaskStats {
            actors,
            total_input_tokens,
            total_output_tokens,
            total_cache_read_tokens,
            total_duration_ms,
            total_cost_usd,
            total_rounds,
        })
    }
}

struct RunInfo {
    run_id: String,
    elapsed_ms: f64,
    end_ts: Option<f64>,
}

// ---------------------------------------------------------------------------
// Round-event parsing helpers
// ---------------------------------------------------------------------------

fn thinking_entry(thinking_length: u64) -> RoundEventEntry {
    RoundEventEntry {
        entry_type: "thinking".to_string(),
        thinking_length: Some(thinking_length),
        text: None,
        tool_name: None,
        tool_input: None,
        tool_result_preview: None,
        is_error: None,
        duration_ms: None,
        cost_usd: None,
        model: None,
    }
}

fn text_entry(text: Option<String>) -> RoundEventEntry {
    RoundEventEntry {
        entry_type: "text".to_string(),
        thinking_length: None,
        text,
        tool_name: None,
        tool_input: None,
        tool_result_preview: None,
        is_error: None,
        duration_ms: None,
        cost_usd: None,
        model: None,
    }
}

fn tool_use_entry(
    tool_name: Option<String>,
    tool_input: Option<Map<String, Value>>,
) -> RoundEventEntry {
    RoundEventEntry {
        entry_type: "tool_use".to_string(),
        thinking_length: None,
        text: None,
        tool_name,
        tool_input,
        tool_result_preview: None,
        is_error: None,
        duration_ms: None,
        cost_usd: None,
        model: None,
    }
}

fn tool_result_entry(tool_result_preview: String, is_error: Option<bool>) -> RoundEventEntry {
    RoundEventEntry {
        entry_type: "tool_result".to_string(),
        thinking_length: None,
        text: None,
        tool_name: None,
        tool_input: None,
        tool_result_preview: Some(tool_result_preview),
        is_error,
        duration_ms: None,
        cost_usd: None,
        model: None,
    }
}

fn take_chars(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

fn js_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number
            .as_f64()
            .is_some_and(|number| number != 0.0 && !number.is_nan()),
        Value::String(text) => !text.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

fn json_number(value: Option<&Value>) -> Option<f64> {
    value.and_then(Value::as_f64).filter(|n| n.is_finite())
}

fn json_u64(value: Option<&Value>) -> Option<u64> {
    value.and_then(Value::as_u64)
}

/// `Date.parse` for the ISO-8601 timestamps the app writes; milliseconds.
fn parse_ts_ms(text: &str) -> Option<f64> {
    DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|dt| dt.timestamp_millis() as f64)
}

// ---------------------------------------------------------------------------
// Session insight (port of the store-relevant parts of session-insight.ts and
// model-detect.ts — kept private because those modules belong to later waves).
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct KimiUsageRecord {
    time_ms: f64,
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
}

#[derive(Debug, Clone)]
struct KimiSessionInsight {
    records: Vec<KimiUsageRecord>,
    model: Option<String>,
}

fn as_number(value: Option<&Value>) -> f64 {
    value.and_then(Value::as_f64).unwrap_or(0.0)
}

/// Locate every wire.jsonl (main agent + subagents) for a kimi session id.
async fn find_kimi_wire_files(home: &Path, session_id: &str) -> Vec<PathBuf> {
    let base = home.join(".kimi-code").join("sessions");
    let mut results = Vec::new();
    let Ok(mut wd_dirs) = fs::read_dir(&base).await else {
        return results;
    };
    while let Ok(Some(wd)) = wd_dirs.next_entry().await {
        let agents_dir = wd.path().join(session_id).join("agents");
        let Ok(mut agents) = fs::read_dir(&agents_dir).await else {
            continue;
        };
        while let Ok(Some(agent)) = agents.next_entry().await {
            results.push(agent.path().join("wire.jsonl"));
        }
    }
    results
}

/// Parse a kimi session's wire files into usage records (one per LLM step)
/// plus the latest model seen. Returns `None` when nothing was found.
async fn read_kimi_session_insight(home: &Path, session_id: &str) -> Option<KimiSessionInsight> {
    if session_id.is_empty() {
        return None;
    }
    let mut records = Vec::new();
    let mut model: Option<String> = None;
    for file in find_kimi_wire_files(home, session_id).await {
        let Ok(raw) = fs::read_to_string(&file).await else {
            continue;
        };
        for line in raw.split('\n') {
            if !line.contains("\"usage.record\"") {
                continue;
            }
            let Ok(entry) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if entry.get("type").and_then(Value::as_str) != Some("usage.record") {
                continue;
            }
            let Some(usage) = entry.get("usage").and_then(Value::as_object) else {
                continue;
            };
            records.push(KimiUsageRecord {
                time_ms: as_number(entry.get("time")),
                input_tokens: as_number(usage.get("inputOther")) as u64,
                output_tokens: as_number(usage.get("output")) as u64,
                cache_read_tokens: as_number(usage.get("inputCacheRead")) as u64,
            });
            if let Some(entry_model) = entry
                .get("model")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
            {
                model = Some(entry_model.to_string());
            }
        }
    }
    if records.is_empty() && model.is_none() {
        return None;
    }
    Some(KimiSessionInsight { records, model })
}

fn is_safe_session_id(session_id: &str) -> bool {
    !session_id.is_empty()
        && session_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// opencode ≥ new storage: model from the SQLite message table via the sqlite3 CLI.
async fn read_opencode_model_from_db(home: &Path, session_id: &str) -> Option<String> {
    if !is_safe_session_id(session_id) {
        return None;
    }
    let db_path = home.join(".local").join("share").join("opencode").join("opencode.db");
    let query = format!(
        "SELECT json_extract(data,'$.providerID') || '/' || json_extract(data,'$.modelID') FROM message WHERE session_id='{session_id}' AND json_extract(data,'$.role')='assistant' AND json_extract(data,'$.modelID') IS NOT NULL ORDER BY time_created DESC LIMIT 1;"
    );
    let run = tokio::process::Command::new("sqlite3")
        .arg("-readonly")
        .arg(&db_path)
        .arg(&query)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output();
    let output = tokio::time::timeout(std::time::Duration::from_secs(5), run)
        .await
        .ok()?
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let out = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !out.is_empty() && out != "/" {
        Some(out)
    } else {
        None
    }
}

/// opencode old storage: JSON files under storage/message/<sessionId>/.
async fn read_opencode_model_from_files(home: &Path, session_id: &str) -> Option<String> {
    let dir = home
        .join(".local")
        .join("share")
        .join("opencode")
        .join("storage")
        .join("message")
        .join(session_id);
    let Ok(mut files) = fs::read_dir(&dir).await else {
        return None;
    };
    let mut latest_ms = -1.0_f64;
    let mut model: Option<String> = None;
    while let Ok(Some(entry)) = files.next_entry().await {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.ends_with(".json") {
            continue;
        }
        let Ok(raw) = fs::read_to_string(entry.path()).await else {
            continue;
        };
        let Ok(message) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        let model_obj = message.get("model").and_then(Value::as_object);
        let provider_id = model_obj
            .and_then(|model| model.get("providerID"))
            .and_then(Value::as_str);
        let model_id = model_obj
            .and_then(|model| model.get("modelID"))
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty());
        let (Some(provider_id), Some(model_id)) = (provider_id, model_id) else {
            continue;
        };
        let created = as_number(
            message
                .get("time")
                .and_then(Value::as_object)
                .and_then(|time| time.get("created")),
        );
        if created >= latest_ms {
            latest_ms = created;
            model = Some(if provider_id.is_empty() {
                model_id.to_string()
            } else {
                format!("{provider_id}/{model_id}")
            });
        }
    }
    model
}

/// Detect the model an opencode session actually used, from its local session
/// storage. Tries the legacy JSON-file storage first, then the SQLite database.
async fn read_opencode_session_model(home: &Path, session_id: &str) -> Option<String> {
    if session_id.is_empty() {
        return None;
    }
    if let Some(from_files) = read_opencode_model_from_files(home, session_id).await {
        return Some(from_files);
    }
    read_opencode_model_from_db(home, session_id).await
}

// ---------------------------------------------------------------------------
// Model detection from CLI config files (model-detect.ts; the commandKindFor
// helpers come from `launchers.rs`).
// ---------------------------------------------------------------------------

/// Detect the current model for an actor by reading its configuration file;
/// fallback when the model cannot be determined from streaming output events.
async fn detect_model_from_config(home: &Path, actor: &str, command: Option<&str>) -> Option<String> {
    // 1. An explicit -m / --model on the command line always wins, for any CLI.
    if let Some(from_command) = model_from_command_args(command) {
        return Some(from_command);
    }

    // 2. Otherwise branch on the actual CLI kind, not the actor name.
    let command = command.unwrap_or("");
    let kind = command_kind_for(actor, command);
    match kind {
        LauncherCommandKind::NativeOpencode => {
            read_json_model(&home.join(".config").join("opencode").join("opencode.json"), "model")
                .await
        }
        LauncherCommandKind::NativeCodex => {
            if is_wecode_codex_command(command) {
                read_wecode_codex_model(home).await
            } else {
                read_toml_model(&home.join(".codex").join("config.toml"), "model").await
            }
        }
        LauncherCommandKind::NativeKimi => {
            // Kimi Code CLI reads ~/.kimi-code/config.toml; ~/.kimi is the legacy path
            if let Some(primary) =
                read_toml_model(&home.join(".kimi-code").join("config.toml"), "default_model").await
            {
                return Some(primary);
            }
            read_toml_model(&home.join(".kimi").join("config.toml"), "default_model").await
        }
        LauncherCommandKind::NativeClaude => {
            if is_wecode_claude_command(command) {
                read_wecode_claude_model(&home.join(".wecode-cli").join("config.json")).await
            } else {
                read_claude_model(&home.join(".claude").join("settings.json")).await
            }
        }
        // contract: model is not knowable before a run.
        _ => None,
    }
}

/// Extract the model from a launcher command's `-m` / `--model` argument.
fn model_from_command_args(command: Option<&str>) -> Option<String> {
    let command = command?;
    if command.is_empty() {
        return None;
    }
    let clean = split_command(command);
    for (index, part) in clean.iter().enumerate() {
        if let Some(model) = part.strip_prefix("--model=") {
            return if model.is_empty() {
                None
            } else {
                Some(model.to_string())
            };
        }
        if (part == "-m" || part == "--model") && index + 1 < clean.len() {
            let model = &clean[index + 1];
            return if model.is_empty() {
                None
            } else {
                Some(model.clone())
            };
        }
    }
    None
}

/// Read the codex model from ~/.wecode-cli/config.json.
/// Structure: { codex: { model: "thudm-glm-5.2", forceModel: false } }
async fn read_wecode_codex_model(home: &Path) -> Option<String> {
    let raw = fs::read_to_string(home.join(".wecode-cli").join("config.json"))
        .await
        .ok()?;
    let obj = serde_json::from_str::<Value>(&raw).ok()?;
    obj.get("codex")
        .and_then(|codex| codex.get("model"))
        .and_then(Value::as_str)
        .filter(|model| !model.is_empty())
        .map(str::to_string)
}

/// Read the effective WeCode Claude model from ~/.wecode-cli/config.json.
/// Structure: { env: { ANTHROPIC_MODEL: "weibo-glm-5.2[1m]" } }
async fn read_wecode_claude_model(path: &Path) -> Option<String> {
    let raw = fs::read_to_string(path).await.ok()?;
    let obj = serde_json::from_str::<Value>(&raw).ok()?;
    obj.get("env")
        .and_then(|env| env.get("ANTHROPIC_MODEL"))
        .and_then(Value::as_str)
        .filter(|model| !model.is_empty())
        .map(str::to_string)
}

/// Read the effective Claude model from ~/.claude/settings.json.
/// `env.ANTHROPIC_MODEL` (the real model the SDK invokes) takes precedence
/// over the `model` tier alias.
async fn read_claude_model(path: &Path) -> Option<String> {
    let raw = fs::read_to_string(path).await.ok()?;
    let obj = serde_json::from_str::<Value>(&raw).ok()?;
    if let Some(overridden) = obj
        .get("env")
        .and_then(|env| env.get("ANTHROPIC_MODEL"))
        .and_then(Value::as_str)
        .filter(|model| !model.is_empty())
    {
        return Some(overridden.to_string());
    }
    obj.get("model")
        .and_then(Value::as_str)
        .filter(|model| !model.is_empty())
        .map(str::to_string)
}

/// Read a model field from a JSON config file.
async fn read_json_model(path: &Path, field: &str) -> Option<String> {
    let raw = fs::read_to_string(path).await.ok()?;
    let obj = serde_json::from_str::<Value>(&raw).ok()?;
    obj.get(field)
        .and_then(Value::as_str)
        .filter(|model| !model.is_empty())
        .map(str::to_string)
}

/// Extract a top-level string field from a TOML config file, using a regex
/// instead of a full TOML parser (same approach as the Electron edition).
async fn read_toml_model(path: &Path, field: &str) -> Option<String> {
    let raw = fs::read_to_string(path).await.ok()?;
    let pattern = regex::Regex::new(&format!(
        r#"(?m)^{field}\s*=\s*(?:"([^"]*)"|'([^']*)'|(\S+))"#
    ))
    .ok()?;
    let captures = pattern.captures(&raw)?;
    let value = captures
        .get(1)
        .or_else(|| captures.get(2))
        .or_else(|| captures.get(3))?
        .as_str();
    let value = value.trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

// ---------------------------------------------------------------------------
// Free helpers (private module functions in store.ts)
// ---------------------------------------------------------------------------

fn transcript_entry_from_json(value: &Value) -> Option<TranscriptEntry> {
    let row = value.as_object()?;
    let role = normalize_transcript_role(row.get("role"))?;
    let content = row
        .get("content")
        .and_then(Value::as_str)
        .filter(|content| !content.is_empty())?;
    let ts = row
        .get("ts")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let meta = row
        .get("meta")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    // TS `numberValue(row.seq)`: any finite JSON number is kept.
    let seq = row
        .get("seq")
        .and_then(Value::as_f64)
        .filter(|seq| seq.is_finite())
        .map(|seq| seq as u64);
    Some(TranscriptEntry {
        role,
        content: content.to_string(),
        ts,
        round: None,
        meta: Some(meta),
        seq,
    })
}

fn normalize_transcript_role(value: Option<&Value>) -> Option<String> {
    let role = value?.as_str()?.trim().to_lowercase();
    if TRANSCRIPT_ROLES.contains(&role.as_str()) {
        Some(role)
    } else {
        None
    }
}

/// Serializes a `TaskState` to its on-disk JSON shape. Fields that are
/// nullable in the zod schema are always emitted (as `null` when absent),
/// matching the Electron edition whose parsed state keeps explicit nulls;
/// optional-but-not-nullable fields are omitted when absent.
fn state_to_json_value(state: &TaskState) -> Value {
    let mut map = Map::new();
    insert_opt(&mut map, "protocol_version", &state.protocol_version);
    insert_opt(&mut map, "task_id", &state.task_id);
    insert_opt(&mut map, "repo_root", &state.repo_root);
    map.insert(
        "status".to_string(),
        Value::String(state.status.as_str().to_string()),
    );
    map.insert("round".to_string(), Value::from(state.round));
    map.insert(
        "rounds_in_window".to_string(),
        Value::from(state.rounds_in_window.unwrap_or(0)),
    );
    map.insert(
        "next_actor".to_string(),
        Value::String(state.next_actor.clone()),
    );
    insert_nullable(&mut map, "countdown", &state.countdown);
    insert_nullable(&mut map, "active_run", &state.active_run);
    map.insert(
        "instruction_queue".to_string(),
        serde_json::to_value(state.instruction_queue.clone().unwrap_or_default())
            .unwrap_or(Value::Array(Vec::new())),
    );
    insert_nullable(&mut map, "claude_session_id", &state.claude_session_id);
    insert_nullable(&mut map, "codex_thread_id", &state.codex_thread_id);
    insert_nullable(&mut map, "cursor_session_id", &state.cursor_session_id);
    insert_nullable(&mut map, "opencode_session_id", &state.opencode_session_id);
    insert_nullable(&mut map, "kimi_session_id", &state.kimi_session_id);
    insert_opt(&mut map, "context_hash", &state.context_hash);
    map.insert(
        "context_sent".to_string(),
        serde_json::to_value(state.context_sent.clone().unwrap_or_default())
            .unwrap_or(Value::Object(Map::new())),
    );
    insert_opt(&mut map, "event_seq", &state.event_seq);
    insert_opt(&mut map, "transcript_seq", &state.transcript_seq);
    insert_opt(&mut map, "consecutive_failures", &state.consecutive_failures);
    insert_nullable(&mut map, "last_error", &state.last_error);
    insert_opt(&mut map, "created_at", &state.created_at);
    insert_opt(&mut map, "updated_at", &state.updated_at);
    insert_nullable(&mut map, "pending_break", &state.pending_break);
    insert_nullable(&mut map, "break_rejected_by", &state.break_rejected_by);
    insert_nullable(&mut map, "latest_failure", &state.latest_failure);
    insert_nullable(&mut map, "health_check", &state.health_check);
    insert_opt(&mut map, "compact_retries", &state.compact_retries);
    insert_opt(&mut map, "execution_mode", &state.execution_mode);
    insert_opt(&mut map, "queue", &state.queue);
    Value::Object(map)
}

fn insert_opt<T: serde::Serialize>(map: &mut Map<String, Value>, key: &str, value: &Option<T>) {
    if let Some(value) = value {
        if let Ok(value) = serde_json::to_value(value) {
            map.insert(key.to_string(), value);
        }
    }
}

fn insert_nullable<T: serde::Serialize>(map: &mut Map<String, Value>, key: &str, value: &Option<T>) {
    let value = match value {
        Some(value) => serde_json::to_value(value).unwrap_or(Value::Null),
        None => Value::Null,
    };
    map.insert(key.to_string(), value);
}

/// A launcher from a task-creation settings override: every field may be
/// missing, and missing fields fall back to the actor defaults (NOT to the
/// global settings — the Electron edition spreads `{command: undefined, ...}`
/// over the global launcher, clobbering it).
#[derive(Debug, Default)]
struct PartialLauncher {
    command: Option<String>,
    env: Option<HashMap<String, String>>,
    timeout_seconds: Option<u64>,
}

fn launcher_from_partial(actor: &str, partial: &PartialLauncher) -> Launcher {
    let fallback = default_launcher_for(actor);
    Launcher {
        command: partial
            .command
            .as_ref()
            .filter(|command| !command.trim().is_empty())
            .cloned()
            .unwrap_or(fallback.command),
        env: partial.env.clone().unwrap_or(fallback.env),
        timeout_seconds: partial.timeout_seconds.unwrap_or(fallback.timeout_seconds),
    }
}

fn coerce_launcher_overrides(value: Option<&Value>) -> HashMap<String, PartialLauncher> {
    let mut launchers = HashMap::new();
    let Some(overrides) = value.and_then(Value::as_object) else {
        return launchers;
    };
    for (actor, raw) in overrides {
        let Some(candidate) = raw.as_object() else {
            continue;
        };
        launchers.insert(
            actor.clone(),
            PartialLauncher {
                command: candidate
                    .get("command")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                env: candidate.get("env").and_then(Value::as_object).map(|env| {
                    env.iter()
                        .filter_map(|(key, value)| {
                            value.as_str().map(|value| (key.clone(), value.to_string()))
                        })
                        .collect()
                }),
                timeout_seconds: candidate.get("timeout_seconds").and_then(Value::as_u64),
            },
        );
    }
    launchers
}

/// Port of `defaultTaskSettings`: global settings + per-task overrides merged
/// into the `settings.json` document. Returns raw JSON because overrides may
/// carry keys outside the `TaskSettings` type (they round-trip to disk but are
/// stripped by the schema on read).
fn default_task_settings(
    global_settings: &GlobalSettings,
    overrides: Option<&HashMap<String, Value>>,
) -> Value {
    let normalized_global = normalize_global_settings(Some(global_settings));
    let global_launchers = normalized_global.launchers.clone().unwrap_or_default();
    let coerced = coerce_launcher_overrides(overrides.and_then(|o| o.get("launchers")));

    let mut launchers = Map::new();
    for actor in DEFAULT_LAUNCHER_ORDER {
        let launcher = match coerced.get(actor) {
            Some(partial) => launcher_from_partial(actor, partial),
            None => global_launchers
                .get(actor)
                .cloned()
                .unwrap_or_else(|| default_launcher_for(actor)),
        };
        if let Ok(value) = serde_json::to_value(&launcher) {
            launchers.insert(actor.to_string(), value);
        }
    }
    // Extra actors from either source, overrides winning per actor.
    for (actor, launcher) in &global_launchers {
        if launchers.contains_key(actor) {
            continue;
        }
        let launcher = match coerced.get(actor) {
            Some(partial) => launcher_from_partial(actor, partial),
            None => launcher.clone(),
        };
        if let Ok(value) = serde_json::to_value(&launcher) {
            launchers.insert(actor.clone(), value);
        }
    }
    for (actor, partial) in &coerced {
        if launchers.contains_key(actor) {
            continue;
        }
        if let Ok(value) = serde_json::to_value(&launcher_from_partial(actor, partial)) {
            launchers.insert(actor.clone(), value);
        }
    }

    let mut map = Map::new();
    map.insert(
        "protocol_version".to_string(),
        Value::String(
            normalized_global
                .protocol_version
                .clone()
                .unwrap_or_else(|| "1".to_string()),
        ),
    );
    map.insert(
        "flow_policy".to_string(),
        Value::String("claude_then_codex".to_string()),
    );
    map.insert(
        "role_mode".to_string(),
        Value::String("claude_implements".to_string()),
    );
    if let Some(max_consecutive_failures) = normalized_global.max_consecutive_failures {
        map.insert(
            "max_consecutive_failures".to_string(),
            Value::from(max_consecutive_failures),
        );
    }
    map.insert("launchers".to_string(), Value::Object(launchers));
    map.insert(
        "seed_claude_session_id".to_string(),
        Value::String(normalized_global.seed_claude_session_id.clone().unwrap_or_default()),
    );
    map.insert(
        "seed_codex_thread_id".to_string(),
        Value::String(normalized_global.seed_codex_thread_id.clone().unwrap_or_default()),
    );
    map.insert(
        "seed_cursor_session_id".to_string(),
        Value::String(
            normalized_global
                .seed_cursor_session_id
                .clone()
                .unwrap_or_default(),
        ),
    );
    map.insert(
        "seed_opencode_session_id".to_string(),
        Value::String(String::new()),
    );
    map.insert(
        "seed_kimi_session_id".to_string(),
        Value::String(String::new()),
    );

    // Remaining overrides spread last, clobbering the defaults above.
    if let Some(overrides) = overrides {
        for (key, value) in overrides {
            if key == "launchers" {
                continue;
            }
            map.insert(key.clone(), value.clone());
        }
    }
    Value::Object(map)
}

fn default_task_state(
    task_id: &str,
    repo_root: &str,
    settings: &TaskSettings,
    context_text: &str,
    now: &str,
) -> TaskState {
    let initial_actor = settings
        .implementer_actor
        .clone()
        .filter(|actor| !actor.is_empty())
        .unwrap_or_else(|| {
            if settings.role_mode == "codex_implements" {
                "codex".to_string()
            } else {
                "claude".to_string()
            }
        });
    TaskState {
        protocol_version: Some("1".to_string()),
        task_id: Some(task_id.to_string()),
        repo_root: Some(repo_root.to_string()),
        status: TaskStatus::Ready,
        round: 0,
        rounds_in_window: Some(0),
        next_actor: initial_actor,
        countdown: None,
        active_run: None,
        instruction_queue: Some(Vec::new()),
        claude_session_id: None,
        codex_thread_id: None,
        cursor_session_id: None,
        opencode_session_id: None,
        kimi_session_id: None,
        context_hash: Some(sha256_hex(context_text)),
        context_sent: Some(
            ACTORS
                .iter()
                .map(|actor| (actor.to_string(), false))
                .collect(),
        ),
        event_seq: Some(0),
        transcript_seq: Some(0),
        consecutive_failures: Some(0),
        last_error: None,
        created_at: Some(now.to_string()),
        updated_at: Some(now.to_string()),
        pending_break: None,
        break_rejected_by: None,
        latest_failure: None,
        health_check: None,
        compact_retries: None,
        execution_mode: Some(ExecutionMode::Immediate),
        queue: None,
    }
}

fn task_markdown_content(value: &str) -> String {
    format!("{}\n", value.trim_end())
}

fn context_markdown_content(value: &str) -> String {
    let trimmed = value.trim_end();
    if trimmed.is_empty() {
        String::new()
    } else {
        format!("{trimmed}\n")
    }
}

fn sha256_hex(value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// `new Date().toISOString().replace(/\.\d{3}Z$/, 'Z')` — second precision.
fn utc_now() -> String {
    Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// `qi_${Date.now()}_${Math.random().toString(16).slice(2, 8)}`
fn new_queue_item_id() -> String {
    format!("qi_{}_{}", Utc::now().timestamp_millis(), random_hex(6))
}

fn random_hex(len: usize) -> String {
    let bytes = uuid::Uuid::new_v4();
    let mut hex = String::with_capacity(len);
    for byte in bytes.as_bytes().iter() {
        hex.push_str(&format!("{byte:02x}"));
        if hex.len() >= len {
            break;
        }
    }
    hex.truncate(len);
    hex
}

/// `JSON.stringify` with object keys sorted recursively (and `undefined`
/// dropped, which serde already guarantees by construction).
fn sort_json_value(value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(sort_json_value).collect()),
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let mut sorted = Map::new();
            for key in keys {
                sorted.insert(key.clone(), sort_json_value(&map[key]));
            }
            Value::Object(sorted)
        }
        other => other.clone(),
    }
}

/// Pretty (2-space) JSON document with sorted keys and a trailing newline.
fn stringify_json(value: &Value) -> String {
    format!(
        "{}\n",
        serde_json::to_string_pretty(&sort_json_value(value)).unwrap_or_default()
    )
}

/// Compact single-line JSON with sorted keys.
fn stringify_json_line(value: &Value) -> String {
    serde_json::to_string(&sort_json_value(value)).unwrap_or_default()
}

/// Python `json.dumps`-style separators (`, ` / `: `) with sorted keys, used
/// for `transcript.jsonl` rows to stay byte-compatible with buddy-python.
fn stringify_python_json_line(value: &Value) -> String {
    stringify_python_json_value(&sort_json_value(value))
}

fn stringify_python_json_value(value: &Value) -> String {
    match value {
        Value::Array(items) => format!(
            "[{}]",
            items
                .iter()
                .map(stringify_python_json_value)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Object(map) => {
            let entries = map
                .iter()
                .map(|(key, item)| {
                    format!(
                        "{}: {}",
                        serde_json::to_string(key).unwrap_or_default(),
                        stringify_python_json_value(item)
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            format!("{{{entries}}}")
        }
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

#[derive(Debug)]
enum ReadJsonError {
    Io(std::io::Error),
    Parse(serde_json::Error),
}

impl From<ReadJsonError> for StoreError {
    fn from(error: ReadJsonError) -> Self {
        match error {
            ReadJsonError::Io(error) => StoreError::Io(error),
            ReadJsonError::Parse(error) => StoreError::Invalid(error.to_string()),
        }
    }
}

async fn read_json(path: &Path) -> Result<Value, ReadJsonError> {
    let text = fs::read_to_string(path).await.map_err(ReadJsonError::Io)?;
    serde_json::from_str(&text).map_err(ReadJsonError::Parse)
}

async fn read_optional_json(path: &Path) -> Value {
    read_json(path).await.unwrap_or(Value::Object(Map::new()))
}

async fn read_optional_text(path: &Path) -> String {
    fs::read_to_string(path).await.unwrap_or_default()
}

async fn list_directory_names(path: &Path) -> Vec<String> {
    let mut names = Vec::new();
    let Ok(mut entries) = fs::read_dir(path).await else {
        return names;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        if entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
            names.push(entry.file_name().to_string_lossy().to_string());
        }
    }
    names
}

async fn directory_exists(path: &Path) -> bool {
    fs::metadata(path).await.is_ok()
}

async fn atomic_write_json(path: &Path, value: &Value) -> Result<(), StoreError> {
    atomic_write_text(path, &stringify_json(value)).await
}

async fn atomic_write_text(path: &Path, value: &str) -> Result<(), StoreError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }
    let tmp = tmp_path_for(path);
    fs::write(&tmp, value).await?;
    fs::rename(&tmp, path).await?;
    Ok(())
}

/// `<file>.tmp-<millis>-<randomhex>`, same convention as the Electron edition.
fn tmp_path_for(path: &Path) -> PathBuf {
    PathBuf::from(format!(
        "{}.tmp-{}-{}",
        path.display(),
        Utc::now().timestamp_millis(),
        random_hex(8)
    ))
}

async fn append_event_line(path: &Path, event: &Value) -> Result<(), StoreError> {
    atomic_append_text(path, &format!("{}\n", stringify_json_line(event))).await
}

async fn atomic_append_text(path: &Path, value: &str) -> Result<(), StoreError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await?;
    file.write_all(value.as_bytes()).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests — ports of tests/unit/main/buddy-store.test.ts,
// buddy-store-write.test.ts and buddy-store-kimi-usage.test.ts.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn store(root: &TempDir) -> BuddyStore {
        BuddyStore::new(root.path())
    }

    async fn write_file(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await.unwrap();
        }
        fs::write(path, contents).await.unwrap();
    }

    fn default_settings_json() -> String {
        json!({
            "protocol_version": "1",
            "countdown_seconds": 30,
            "flow_policy": "claude_then_codex",
            "role_mode": "claude_implements",
            "launchers": {}
        })
        .to_string()
    }

    // --- buddy-store.test.ts: creates buddy-python compatible initial state ---

    #[tokio::test]
    async fn creates_python_compatible_initial_state_from_implementer_settings() {
        let root = TempDir::new().unwrap();
        let store = store(&root);

        let mut settings = HashMap::new();
        settings.insert("role_mode".to_string(), json!("codex_implements"));
        settings.insert("implementer_actor".to_string(), json!("opencode"));
        settings.insert("reviewer_actor".to_string(), json!("kimi"));
        settings.insert("launchers".to_string(), json!({}));

        let created = store
            .create_task(CreateTaskInput {
                task_id: "demo".to_string(),
                repo_root: Some("/tmp/repo".to_string()),
                task_text: Some("# Demo".to_string()),
                context_text: Some("background".to_string()),
                settings: Some(settings),
                execution_mode: None,
            })
            .await
            .unwrap();

        let detail = store
            .get_task_detail("demo", &created.workspace_key)
            .await
            .unwrap();

        assert_eq!(detail.state.round, 0);
        assert_eq!(detail.state.rounds_in_window, Some(0));
        assert_eq!(detail.state.next_actor, "opencode");
        let context_hash = detail.state.context_hash.unwrap();
        assert_eq!(context_hash.len(), 64);
        assert!(context_hash.chars().all(|c| c.is_ascii_hexdigit()));
        let context_sent = detail.state.context_sent.unwrap();
        for actor in ACTORS {
            assert_eq!(context_sent.get(actor), Some(&false));
        }
        assert!(detail.state.countdown.is_none());
        assert!(detail.state.last_error.is_none());
    }

    // --- buddy-store.test.ts: loads tasks and task detail -------------------

    #[tokio::test]
    async fn loads_tasks_and_task_detail_from_data_directory() {
        let root = TempDir::new().unwrap();
        let task_dir = root
            .path()
            .join("workspaces")
            .join("abc123def456")
            .join("tasks")
            .join("demo");
        fs::create_dir_all(&task_dir).await.unwrap();
        write_file(&task_dir.join("settings.json"), &default_settings_json()).await;
        write_file(
            &task_dir.join("state.json"),
            &json!({
                "status": "READY",
                "round": 1,
                "next_actor": "claude",
                "active_run": null,
                "updated_at": "2026-05-26T00:00:00.000Z",
                "repo_root": "/tmp/repo"
            })
            .to_string(),
        )
        .await;
        write_file(
            &task_dir.join("task.json"),
            &json!({ "task_text": "Build it", "context_text": "Use tests" }).to_string(),
        )
        .await;
        write_file(&task_dir.join("transcript.md"), "hello transcript").await;
        write_file(
            &task_dir.join("events.jsonl"),
            &[
                "{\"seq\":1,\"type\":\"task.created\",\"ts\":\"2026-05-26T00:00:00.000Z\",\"payload\":{}}",
                "{\"seq\":2,\"type\":\"message.added\",\"ts\":\"2026-05-26T00:01:00.000Z\",\"payload\":{\"message\":\"Please adjust\"}}",
                "{\"seq\":3,\"type\":\"actor.completed\",\"actor\":\"codex\",\"ts\":\"2026-05-26T00:02:00.000Z\",\"payload\":{\"text\":\"Done\"}}",
                "",
            ]
            .join("\n"),
        )
        .await;

        let store = store(&root);

        let tasks = store.get_tasks().await;
        assert_eq!(tasks.len(), 1);
        let task = &tasks[0];
        assert_eq!(task.task_id, "demo");
        assert_eq!(task.workspace_key, "abc123def456");
        assert_eq!(task.status, TaskStatus::Ready);
        assert_eq!(task.repo_root, "/tmp/repo");

        let detail = store.get_task_detail("demo", "abc123def456").await.unwrap();
        assert_eq!(detail.task_id, "demo");
        assert_eq!(detail.workspace_key, "abc123def456");
        assert_eq!(detail.task_text, "Build it");
        assert_eq!(detail.context_text, "Use tests");
        assert!(detail.transcript.is_empty());
        let seqs: Vec<u64> = detail.events.iter().map(|event| event.seq).collect();
        assert_eq!(seqs, vec![1, 2, 3]);
    }

    // --- buddy-store.test.ts: no transcript from events ----------------------

    #[tokio::test]
    async fn does_not_derive_chat_transcript_from_events_without_transcript_jsonl() {
        let root = TempDir::new().unwrap();
        let task_dir = root
            .path()
            .join("workspaces")
            .join("abc123def456")
            .join("tasks")
            .join("demo");
        fs::create_dir_all(&task_dir).await.unwrap();
        write_file(&task_dir.join("settings.json"), &default_settings_json()).await;
        write_file(
            &task_dir.join("state.json"),
            &json!({
                "status": "READY",
                "round": 1,
                "next_actor": "claude",
                "active_run": null
            })
            .to_string(),
        )
        .await;
        write_file(
            &task_dir.join("task.json"),
            &json!({ "task_text": "Build it", "context_text": "" }).to_string(),
        )
        .await;
        write_file(
            &task_dir.join("events.jsonl"),
            &[
                "{\"seq\":1,\"type\":\"human.message\",\"ts\":\"2026-05-26T00:01:00.000Z\",\"payload\":{\"content\":\"More context\"}}",
                "{\"seq\":2,\"type\":\"assistant\",\"actor\":\"claude\",\"ts\":\"2026-05-26T00:02:00.000Z\",\"payload\":{\"message\":{\"content\":[{\"type\":\"thinking\",\"thinking\":\"hidden\"},{\"type\":\"text\",\"text\":\"```json\\n{\\\"type\\\":\\\"chat\\\",\\\"content\\\":\\\"I will do it\\\"}\\n```\"}]}}}",
                "",
            ]
            .join("\n"),
        )
        .await;

        let store = store(&root);
        let detail = store.get_task_detail("demo", "abc123def456").await.unwrap();
        assert!(detail.transcript.is_empty());
    }

    // --- buddy-store.test.ts: transcript jsonl is the source of truth --------

    #[tokio::test]
    async fn loads_transcript_jsonl_as_the_conversation_source_of_truth() {
        let root = TempDir::new().unwrap();
        let task_dir = root
            .path()
            .join("workspaces")
            .join("abc123def456")
            .join("tasks")
            .join("demo");
        fs::create_dir_all(&task_dir).await.unwrap();
        write_file(&task_dir.join("settings.json"), &default_settings_json()).await;
        write_file(
            &task_dir.join("state.json"),
            &json!({
                "status": "DONE",
                "round": 2,
                "next_actor": "claude",
                "active_run": null
            })
            .to_string(),
        )
        .await;
        write_file(
            &task_dir.join("task.json"),
            &json!({ "task_text": "Build it", "context_text": "" }).to_string(),
        )
        .await;
        write_file(
            &task_dir.join("transcript.jsonl"),
            &[
                "{\"seq\":1,\"ts\":\"2026-05-26T00:01:00.000Z\",\"role\":\"claude\",\"content\":\"Claude final\",\"meta\":{\"buddy_type\":\"chat\",\"round\":1,\"run_id\":\"run-001\",\"elapsed_ms\":1000}}",
                "{\"seq\":2,\"ts\":\"2026-05-26T00:02:00.000Z\",\"role\":\"system\",\"content\":\"Claude Code 请求结束任务，等待 Codex 确认。\",\"meta\":{\"kind\":\"round_notice\",\"round\":2}}",
                "",
            ]
            .join("\n"),
        )
        .await;
        write_file(
            &task_dir.join("events.jsonl"),
            "{\"seq\":1,\"type\":\"assistant\",\"actor\":\"claude\",\"ts\":\"2026-05-26T00:00:30.000Z\",\"run_id\":\"run-001\",\"payload\":{\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"Started run.\"}]}}}\n",
        )
        .await;

        let store = store(&root);
        let detail = store.get_task_detail("demo", "abc123def456").await.unwrap();

        assert_eq!(detail.transcript.len(), 2);
        let first = &detail.transcript[0];
        assert_eq!(first.role, "claude");
        assert_eq!(first.content, "Claude final");
        let first_meta = first.meta.as_ref().unwrap();
        assert_eq!(first_meta.get("buddy_type"), Some(&json!("chat")));
        assert_eq!(first_meta.get("round"), Some(&json!(1)));
        assert_eq!(first_meta.get("elapsed_ms"), Some(&json!(1000)));
        let second = &detail.transcript[1];
        assert_eq!(second.role, "system");
        assert_eq!(second.content, "Claude Code 请求结束任务，等待 Codex 确认。");
        let second_meta = second.meta.as_ref().unwrap();
        assert_eq!(second_meta.get("kind"), Some(&json!("round_notice")));
        assert_eq!(second_meta.get("round"), Some(&json!(2)));
    }

    // --- buddy-store.test.ts: no transcript from legacy final actor events ---

    #[tokio::test]
    async fn does_not_derive_chat_transcript_from_legacy_final_actor_events() {
        let root = TempDir::new().unwrap();
        let task_dir = root
            .path()
            .join("workspaces")
            .join("abc123def456")
            .join("tasks")
            .join("demo");
        fs::create_dir_all(&task_dir).await.unwrap();
        write_file(&task_dir.join("settings.json"), &default_settings_json()).await;
        write_file(
            &task_dir.join("state.json"),
            &json!({
                "status": "DONE",
                "round": 5,
                "next_actor": "codex",
                "active_run": null
            })
            .to_string(),
        )
        .await;
        write_file(
            &task_dir.join("task.json"),
            &json!({ "task_text": "Build it", "context_text": "" }).to_string(),
        )
        .await;
        write_file(
            &task_dir.join("events.jsonl"),
            &[
                "{\"seq\":1,\"type\":\"assistant\",\"actor\":\"claude\",\"ts\":\"2026-05-26T00:01:00.000Z\",\"run_id\":\"run-003\",\"payload\":{\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"现在验证类型检查。\"}]}}}",
                "{\"seq\":2,\"type\":\"result\",\"actor\":\"claude\",\"ts\":\"2026-05-26T00:01:10.000Z\",\"run_id\":\"run-003\",\"payload\":{\"result\":\"```json\\n{\\\"type\\\":\\\"chat\\\",\\\"content\\\":\\\"Claude final\\\"}\\n```\"}}",
                "{\"seq\":3,\"type\":\"actor.finished\",\"actor\":\"claude\",\"ts\":\"2026-05-26T00:01:10.000Z\",\"run_id\":\"run-003\",\"payload\":{\"elapsed_ms\":38000}}",
                "{\"seq\":4,\"type\":\"item.completed\",\"actor\":\"codex\",\"ts\":\"2026-05-26T00:03:00.000Z\",\"run_id\":\"run-004\",\"payload\":{\"item\":{\"type\":\"agent_message\",\"text\":\"{\\\"type\\\":\\\"break\\\",\\\"content\\\":\\\"Codex final\\\"}\"}}}",
                "{\"seq\":5,\"type\":\"actor.finished\",\"actor\":\"codex\",\"ts\":\"2026-05-26T00:03:00.000Z\",\"run_id\":\"run-004\",\"payload\":{\"elapsed_ms\":119835}}",
                "{\"seq\":6,\"type\":\"break.pending\",\"actor\":\"codex\",\"ts\":\"2026-05-26T00:03:00.000Z\",\"run_id\":\"run-004\",\"payload\":{\"pending_confirmation_from\":\"claude\"}}",
                "{\"seq\":7,\"type\":\"actor.started\",\"actor\":\"claude\",\"ts\":\"2026-05-26T00:03:30.000Z\",\"run_id\":\"run-005\",\"payload\":{\"message\":\"Started run.\"}}",
                "{\"seq\":8,\"type\":\"result\",\"actor\":\"claude\",\"ts\":\"2026-05-26T00:03:35.000Z\",\"run_id\":\"run-005\",\"payload\":{\"result\":\"{\\\"type\\\":\\\"break\\\",\\\"content\\\":\\\"Claude confirms\\\"}\"}}",
                "{\"seq\":9,\"type\":\"actor.finished\",\"actor\":\"claude\",\"ts\":\"2026-05-26T00:03:35.000Z\",\"run_id\":\"run-005\",\"payload\":{\"elapsed_ms\":5031}}",
                "{\"seq\":10,\"type\":\"task.done\",\"ts\":\"2026-05-26T00:03:35.000Z\",\"payload\":{\"first_actor\":\"codex\",\"second_actor\":\"claude\",\"round\":5,\"reason\":\"dual_break_confirmed\"}}",
                "",
            ]
            .join("\n"),
        )
        .await;

        let store = store(&root);
        let detail = store.get_task_detail("demo", "abc123def456").await.unwrap();
        assert!(detail.transcript.is_empty());
    }

    // --- buddy-store.test.ts: no fallback to transcript markdown -------------

    #[tokio::test]
    async fn does_not_fall_back_to_transcript_markdown_for_chat_transcript() {
        let root = TempDir::new().unwrap();
        let task_dir = root
            .path()
            .join("workspaces")
            .join("abc123def456")
            .join("tasks")
            .join("demo");
        fs::create_dir_all(&task_dir).await.unwrap();
        write_file(&task_dir.join("settings.json"), &default_settings_json()).await;
        write_file(
            &task_dir.join("state.json"),
            &json!({
                "status": "READY",
                "round": 1,
                "next_actor": "claude",
                "active_run": null
            })
            .to_string(),
        )
        .await;
        write_file(
            &task_dir.join("task.json"),
            &json!({ "task_text": "Build it", "context_text": "" }).to_string(),
        )
        .await;
        write_file(
            &task_dir.join("transcript.md"),
            &[
                "# demo",
                "",
                "## Task",
                "Build it",
                "",
                "## Human",
                "Please continue",
                "",
                "## Claude",
                "Continuing now",
                "",
            ]
            .join("\n"),
        )
        .await;
        write_file(
            &task_dir.join("events.jsonl"),
            "{\"seq\":1,\"type\":\"task.created\",\"ts\":\"2026-05-26T00:00:00.000Z\",\"payload\":{}}\n",
        )
        .await;

        let store = store(&root);
        let detail = store.get_task_detail("demo", "abc123def456").await.unwrap();
        assert!(detail.transcript.is_empty());
    }

    // --- buddy-store.test.ts: legacy markdown tasks --------------------------

    #[tokio::test]
    async fn loads_legacy_markdown_tasks_with_nullable_state_fields() {
        let root = TempDir::new().unwrap();
        let workspace_dir = root.path().join("workspaces").join("buddy-macos-31bd2c697ab4");
        let task_dir = workspace_dir.join("tasks").join("设置页基本功能");
        fs::create_dir_all(&task_dir).await.unwrap();
        write_file(
            &workspace_dir.join("workspace.json"),
            &json!({ "default_repo_root": "/tmp/buddy-macos" }).to_string(),
        )
        .await;
        write_file(&task_dir.join("settings.json"), &default_settings_json()).await;
        write_file(
            &task_dir.join("state.json"),
            &json!({
                "status": "PAUSED",
                "round": 0,
                "next_actor": "claude",
                "active_run": null,
                "countdown": null,
                "claude_session_id": null,
                "codex_thread_id": null,
                "updated_at": "2026-05-25T08:56:45Z",
                "repo_root": "/tmp/buddy-macos"
            })
            .to_string(),
        )
        .await;
        write_file(&task_dir.join("task.md"), "Legacy task text").await;
        write_file(&task_dir.join("context.md"), "Legacy context text").await;
        write_file(
            &task_dir.join("events.jsonl"),
            "{\"seq\":1,\"type\":\"task.created\",\"ts\":\"2026-05-25T08:00:00.000Z\",\"payload\":{}}\n",
        )
        .await;

        let store = store(&root);

        let tasks = store.get_tasks().await;
        assert_eq!(tasks.len(), 1);
        let task = &tasks[0];
        assert_eq!(task.task_id, "设置页基本功能");
        assert_eq!(task.workspace_key, "buddy-macos-31bd2c697ab4");
        assert_eq!(task.status, TaskStatus::Paused);
        assert_eq!(task.repo_root, "/tmp/buddy-macos");

        let detail = store
            .get_task_detail("设置页基本功能", "buddy-macos-31bd2c697ab4")
            .await
            .unwrap();
        assert_eq!(detail.task_text, "Legacy task text");
        assert_eq!(detail.context_text, "Legacy context text");
    }

    // --- buddy-store-write.test.ts: file layout and initial contents ---------

    #[tokio::test]
    async fn creates_task_using_python_file_layout_and_initial_contents() {
        let root = TempDir::new().unwrap();
        let repo_root = tempfile::Builder::new()
            .prefix("buddy-write-repo-")
            .tempdir()
            .unwrap();
        let store = store(&root);

        let result = store
            .create_task(CreateTaskInput {
                task_id: "demo".to_string(),
                repo_root: Some(repo_root.path().to_string_lossy().to_string()),
                task_text: Some("Build it\n\n".to_string()),
                context_text: Some("Use tests\n\n".to_string()),
                settings: None,
                execution_mode: None,
            })
            .await
            .unwrap();
        let expected_repo_root = std::fs::canonicalize(repo_root.path())
            .unwrap()
            .to_string_lossy()
            .to_string();

        let task_dir = root
            .path()
            .join("workspaces")
            .join(&result.workspace_key)
            .join("tasks")
            .join("demo");
        assert!(
            result.workspace_key.starts_with("buddy-write-repo-"),
            "workspace key: {}",
            result.workspace_key
        );
        let suffix = result.workspace_key.rsplit('-').next().unwrap();
        assert_eq!(suffix.len(), 12);
        assert!(suffix.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(result.path, task_dir.to_string_lossy().to_string());

        let workspace_json =
            fs::read_to_string(root.path().join("workspaces").join(&result.workspace_key).join("workspace.json"))
                .await
                .unwrap();
        assert!(
            workspace_json.contains(&format!("\"default_repo_root\": \"{expected_repo_root}\"")),
            "workspace.json: {workspace_json}"
        );
        assert_eq!(
            fs::read_to_string(task_dir.join("task.md")).await.unwrap(),
            "Build it\n"
        );
        assert_eq!(
            fs::read_to_string(task_dir.join("context.md")).await.unwrap(),
            "Use tests\n"
        );
        assert_eq!(
            fs::read_to_string(task_dir.join("status")).await.unwrap(),
            "READY\n"
        );
        assert!(task_dir.join("rounds").is_dir());
        assert!(task_dir.join("artifacts").is_dir());
        assert!(task_dir.join(".buddy.lock").exists());
        assert!(!task_dir.join("task.json").exists());
        assert!(!task_dir.join("transcript.md").exists());
        assert!(!task_dir.join("transcript.jsonl").exists());

        let settings: Value = serde_json::from_str(
            &fs::read_to_string(task_dir.join("settings.json")).await.unwrap(),
        )
        .unwrap();
        assert_eq!(settings["protocol_version"], json!("1"));
        assert_eq!(settings["flow_policy"], json!("claude_then_codex"));
        assert_eq!(settings["role_mode"], json!("claude_implements"));
        assert_eq!(settings["max_consecutive_failures"], json!(10));
        assert_eq!(settings["seed_claude_session_id"], json!(""));
        assert_eq!(settings["seed_codex_thread_id"], json!(""));
        assert_eq!(
            settings["launchers"]["claude"],
            json!({ "command": "claude", "env": {}, "timeout_seconds": 7200 })
        );
        assert_eq!(
            settings["launchers"]["codex"],
            json!({ "command": "codex", "env": {}, "timeout_seconds": 7200 })
        );

        let state: Value =
            serde_json::from_str(&fs::read_to_string(task_dir.join("state.json")).await.unwrap())
                .unwrap();
        assert_eq!(state["protocol_version"], json!("1"));
        assert_eq!(state["task_id"], json!("demo"));
        assert_eq!(state["repo_root"], json!(expected_repo_root));
        assert_eq!(state["status"], json!("READY"));
        assert_eq!(state["round"], json!(0));
        assert_eq!(state["rounds_in_window"], json!(0));
        assert_eq!(state["next_actor"], json!("claude"));
        assert!(state["claude_session_id"].is_null());
        assert!(state["codex_thread_id"].is_null());
        assert_eq!(state["context_sent"]["claude"], json!(false));
        assert_eq!(state["context_sent"]["codex"], json!(false));
        assert!(state["active_run"].is_null());
        assert!(state["countdown"].is_null());
        assert!(state["last_error"].is_null());
        assert_eq!(state["event_seq"], json!(1));
        assert_eq!(state["transcript_seq"], json!(0));
        assert_eq!(state["consecutive_failures"], json!(0));
        let context_hash = state["context_hash"].as_str().unwrap();
        assert_eq!(context_hash.len(), 64);
        assert!(context_hash.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(state["created_at"].as_str().unwrap().ends_with('Z'));
        assert!(state["updated_at"].as_str().unwrap().ends_with('Z'));

        let events_text = fs::read_to_string(task_dir.join("events.jsonl")).await.unwrap();
        let event: Value = serde_json::from_str(events_text.trim()).unwrap();
        assert_eq!(event["payload"]["task_id"], json!("demo"));
        assert_eq!(event["seq"], json!(1));
        assert_eq!(event["task_id"], json!("demo"));
        assert_eq!(event["type"], json!("task.created"));
    }

    // --- buddy-store-write.test.ts: global CLI settings -----------------------

    #[tokio::test]
    async fn uses_global_cli_settings_when_creating_task_without_explicit_launchers() {
        let root = TempDir::new().unwrap();
        let store = store(&root);
        let mut env = HashMap::new();
        env.insert("BUDDY_MODE".to_string(), "native".to_string());
        let mut launchers = HashMap::new();
        launchers.insert(
            "codex".to_string(),
            Launcher {
                command: "codex --profile native".to_string(),
                env,
                timeout_seconds: 123,
            },
        );
        store
            .update_global_settings(&GlobalSettings {
                countdown_seconds: Some(12),
                launchers: Some(launchers),
                ..GlobalSettings::default()
            })
            .await
            .unwrap();

        let result = store
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

        let task_dir = root
            .path()
            .join("workspaces")
            .join(&result.workspace_key)
            .join("tasks")
            .join("demo");
        let settings: Value = serde_json::from_str(
            &fs::read_to_string(task_dir.join("settings.json")).await.unwrap(),
        )
        .unwrap();
        assert_eq!(
            settings["launchers"]["codex"],
            json!({
                "command": "codex --profile native",
                "env": { "BUDDY_MODE": "native" },
                "timeout_seconds": 123
            })
        );
    }

    // --- buddy-store-write.test.ts: task id deduplication ---------------------

    #[tokio::test]
    async fn deduplicates_task_ids_by_appending_numeric_suffixes() {
        let root = TempDir::new().unwrap();
        let repo_root = TempDir::new().unwrap();
        let store = store(&root);
        let input = || CreateTaskInput {
            task_id: "demo".to_string(),
            repo_root: Some(repo_root.path().to_string_lossy().to_string()),
            task_text: None,
            context_text: None,
            settings: None,
            execution_mode: None,
        };

        let first = store.create_task(input()).await.unwrap();
        assert_eq!(first.task, "demo");
        let second = store.create_task(input()).await.unwrap();
        assert_eq!(second.task, "demo_2");
        let third = store.create_task(input()).await.unwrap();
        assert_eq!(third.task, "demo_3");

        for id in ["demo", "demo_2", "demo_3"] {
            assert!(
                root.path()
                    .join("workspaces")
                    .join(&first.workspace_key)
                    .join("tasks")
                    .join(id)
                    .join("settings.json")
                    .exists(),
                "missing settings.json for {id}"
            );
        }
    }

    // --- buddy-store-write.test.ts: transcript jsonl formatting ---------------

    #[tokio::test]
    async fn appends_transcript_rows_using_python_jsonl_formatting_and_state_sequence() {
        let root = TempDir::new().unwrap();
        let store = store(&root);
        let created = store
            .create_task(CreateTaskInput {
                task_id: "demo".to_string(),
                repo_root: Some(root.path().to_string_lossy().to_string()),
                task_text: None,
                context_text: None,
                settings: None,
                execution_mode: None,
            })
            .await
            .unwrap();
        let task_dir = root
            .path()
            .join("workspaces")
            .join(&created.workspace_key)
            .join("tasks")
            .join("demo");

        write_file(
            &task_dir.join("transcript.jsonl"),
            "{\"content\": \"legacy\", \"meta\": {}, \"role\": \"human\", \"seq\": 99, \"ts\": \"2026-05-26T00:00:00Z\"}\n",
        )
        .await;

        let mut meta1 = Map::new();
        meta1.insert("source".to_string(), json!("run_once"));
        store
            .append_transcript("demo", &created.workspace_key, "human", "补充一下", meta1)
            .await
            .unwrap();
        let mut meta2 = Map::new();
        meta2.insert("round".to_string(), json!(1));
        meta2.insert("run_id".to_string(), json!("run-001"));
        meta2.insert("elapsed_ms".to_string(), json!(12));
        meta2.insert("buddy_type".to_string(), json!("chat"));
        store
            .append_transcript(
                "demo",
                &created.workspace_key,
                "codex",
                "## 结果\n\n- 完成: yes\n- 路径: `src/main`\n",
                meta2,
            )
            .await
            .unwrap();

        let text = fs::read_to_string(task_dir.join("transcript.jsonl")).await.unwrap();
        let lines: Vec<&str> = text.trim_end().split('\n').collect();
        assert_eq!(lines.len(), 3);
        let pattern1 = regex::Regex::new(
            r#"^\{"content": "补充一下", "meta": \{"source": "run_once"\}, "role": "human", "seq": 1, "ts": ".*Z"\}$"#,
        )
        .unwrap();
        assert!(pattern1.is_match(lines[1]), "line 1: {}", lines[1]);
        let pattern2 = regex::Regex::new(
            r###"^\{"content": "## 结果\\n\\n- 完成: yes\\n- 路径: `src/main`\\n", "meta": \{"buddy_type": "chat", "elapsed_ms": 12, "round": 1, "run_id": "run-001"\}, "role": "codex", "seq": 2, "ts": ".*Z"\}$"###,
        )
        .unwrap();
        assert!(pattern2.is_match(lines[2]), "line 2: {}", lines[2]);

        let state: Value =
            serde_json::from_str(&fs::read_to_string(task_dir.join("state.json")).await.unwrap())
                .unwrap();
        assert_eq!(state["transcript_seq"], json!(2));
    }

    // --- buddy-store-kimi-usage.test.ts ---------------------------------------

    const SESSION_ID: &str = "session_aaaa-bbbb";
    const WORKSPACE_KEY: &str = "abc123def456";
    const RUN_ID: &str = "run_1784561664618_567073";

    async fn setup_kimi_task(root: &Path) {
        let task_dir = root
            .join("workspaces")
            .join(WORKSPACE_KEY)
            .join("tasks")
            .join("demo");
        fs::create_dir_all(task_dir.join("artifacts")).await.unwrap();
        write_file(
            &task_dir.join("settings.json"),
            &json!({
                "protocol_version": "1",
                "countdown_seconds": 30,
                "flow_policy": "claude_then_codex",
                "role_mode": "kimi_implements",
                "launchers": {}
            })
            .to_string(),
        )
        .await;
        write_file(
            &task_dir.join("state.json"),
            &json!({
                "status": "DONE",
                "round": 1,
                "next_actor": "kimi",
                "repo_root": "/tmp/repo",
                "kimi_session_id": SESSION_ID
            })
            .to_string(),
        )
        .await;
        // kimi events: role-based lines without usage/model (as the CLI actually emits)
        write_file(
            &task_dir.join("artifacts").join(format!("{RUN_ID}-events.jsonl")),
            &[
                json!({ "role": "assistant", "content": "{\"type\":\"chat\",\"content\":\"done\"}" })
                    .to_string(),
                json!({ "role": "meta", "type": "session.resume_hint", "session_id": SESSION_ID })
                    .to_string(),
            ]
            .join("\n"),
        )
        .await;
        // transcript: one kimi run ending at 15:34:57Z after 32579ms
        write_file(
            &task_dir.join("transcript.jsonl"),
            &json!({
                "role": "kimi",
                "content": "...",
                "ts": "2026-07-20T15:34:57.000Z",
                "meta": { "run_id": RUN_ID, "elapsed_ms": 32579, "round": 1 }
            })
            .to_string(),
        )
        .await;
    }

    fn usage_record(time_ms: i64, input_other: u64, output: u64, cache_read: u64) -> String {
        json!({
            "type": "usage.record",
            "model": "kimi-code/k3",
            "usage": {
                "inputOther": input_other,
                "output": output,
                "inputCacheRead": cache_read,
                "inputCacheCreation": 0
            },
            "usageScope": "turn",
            "time": time_ms
        })
        .to_string()
    }

    fn ts_ms(text: &str) -> i64 {
        DateTime::parse_from_rfc3339(text).unwrap().timestamp_millis()
    }

    #[tokio::test]
    async fn attributes_wire_jsonl_usage_to_the_run_window_and_fills_the_model() {
        let root = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        setup_kimi_task(root.path()).await;
        // Run window: [15:34:57.000 - 32.579s - 5s, 15:34:57.000 + 5s]
        let wire_dir = home
            .path()
            .join(".kimi-code")
            .join("sessions")
            .join("wd_repo_abc123")
            .join(SESSION_ID)
            .join("agents")
            .join("main");
        write_file(
            &wire_dir.join("wire.jsonl"),
            &[
                usage_record(ts_ms("2026-07-20T15:34:40.000Z"), 100, 10, 900),
                usage_record(ts_ms("2026-07-20T15:34:55.000Z"), 200, 20, 800),
                // Outside every run window (e.g. health-check ping) — must not be counted
                usage_record(ts_ms("2026-07-20T15:00:00.000Z"), 999, 999, 999),
            ]
            .join("\n"),
        )
        .await;

        let store = BuddyStore::new(root.path()).with_home_dir(home.path());
        let stats = store.get_task_stats("demo", WORKSPACE_KEY).await.unwrap();

        assert_eq!(stats.actors.len(), 1);
        let kimi = &stats.actors[0];
        assert_eq!(kimi.actor, "kimi");
        assert_eq!(kimi.input_tokens, 300);
        assert_eq!(kimi.output_tokens, 30);
        assert_eq!(kimi.cache_read_tokens, 1700);
        assert_eq!(kimi.model.as_deref(), Some("kimi-code/k3"));
        assert_eq!(stats.total_input_tokens, 300);
        assert_eq!(stats.total_cache_read_tokens, 1700);
    }

    #[tokio::test]
    async fn leaves_tokens_at_zero_when_no_wire_file_exists() {
        let root = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        setup_kimi_task(root.path()).await;

        let store = BuddyStore::new(root.path()).with_home_dir(home.path());
        let stats = store.get_task_stats("demo", WORKSPACE_KEY).await.unwrap();

        assert_eq!(stats.actors[0].input_tokens, 0);
        assert_eq!(stats.actors[0].output_tokens, 0);
    }
}
