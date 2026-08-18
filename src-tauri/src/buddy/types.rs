//! Core data types mirroring `src/shared/types.ts` of the original Electron
//! Buddy app. Field names are serialized to the exact same JSON shape so that
//! on-disk data (`~/Library/Application Support/buddy/`) stays compatible
//! between the Electron and Tauri editions.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum TaskStatus {
    #[serde(rename = "QUEUED")]
    Queued,
    #[serde(rename = "READY")]
    Ready,
    #[serde(rename = "RUNNING_CLAUDE")]
    RunningClaude,
    #[serde(rename = "RUNNING_CODEX")]
    RunningCodex,
    #[serde(rename = "RUNNING_CURSOR")]
    RunningCursor,
    #[serde(rename = "RUNNING_OPENCODE")]
    RunningOpencode,
    #[serde(rename = "RUNNING_KIMI")]
    RunningKimi,
    #[serde(rename = "PINGING")]
    Pinging,
    #[serde(rename = "COUNTDOWN")]
    Countdown,
    #[serde(rename = "PAUSED")]
    Paused,
    #[serde(rename = "FAILED")]
    Failed,
    #[serde(rename = "DONE")]
    Done,
}

impl TaskStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskStatus::Queued => "QUEUED",
            TaskStatus::Ready => "READY",
            TaskStatus::RunningClaude => "RUNNING_CLAUDE",
            TaskStatus::RunningCodex => "RUNNING_CODEX",
            TaskStatus::RunningCursor => "RUNNING_CURSOR",
            TaskStatus::RunningOpencode => "RUNNING_OPENCODE",
            TaskStatus::RunningKimi => "RUNNING_KIMI",
            TaskStatus::Pinging => "PINGING",
            TaskStatus::Countdown => "COUNTDOWN",
            TaskStatus::Paused => "PAUSED",
            TaskStatus::Failed => "FAILED",
            TaskStatus::Done => "DONE",
        }
    }

    /// Status used while a given actor is running.
    pub fn running_for(actor: &str) -> TaskStatus {
        match actor {
            "claude" => TaskStatus::RunningClaude,
            "codex" => TaskStatus::RunningCodex,
            "cursor" => TaskStatus::RunningCursor,
            "opencode" => TaskStatus::RunningOpencode,
            "kimi" => TaskStatus::RunningKimi,
            _ => TaskStatus::Ready,
        }
    }

    pub fn is_running(&self) -> bool {
        matches!(
            self,
            TaskStatus::RunningClaude
                | TaskStatus::RunningCodex
                | TaskStatus::RunningCursor
                | TaskStatus::RunningOpencode
                | TaskStatus::RunningKimi
                | TaskStatus::Pinging
        )
    }
}

impl std::fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ExecutionMode {
    Immediate,
    Queued,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub task_id: String,
    pub workspace_key: String,
    /// Absolute task directory, computed at list time (not persisted).
    pub task_dir: String,
    pub status: TaskStatus,
    pub updated_at: String,
    pub repo_root: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub round: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_run: Option<ActiveRun>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_mode: Option<ExecutionMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue: Option<TaskQueueInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
}

/// Per-project FIFO queue metadata attached to queued-execution tasks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskQueueInfo {
    pub state: String, // 'waiting' | 'active' | 'superseded'
    pub enqueued_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activated_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activation_source: Option<String>, // 'automatic' | 'manual'
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskDetail {
    pub task_id: String,
    pub workspace_key: String,
    pub state: TaskState,
    pub settings: TaskSettings,
    pub task_text: String,
    pub context_text: String,
    pub transcript: Vec<TranscriptEntry>,
    pub events: Vec<Event>,
    pub latest_failure: Option<Failure>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstructionQueueItem {
    pub id: String,
    pub content: String,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachments: Option<Vec<AttachmentMeta>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HealthCheckResult {
    pub actors: HashMap<String, String>, // 'pending' | 'running' | 'passed' | 'failed'
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed_actor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BreakMarker {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub round: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_root: Option<String>,
    pub status: TaskStatus,
    pub round: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rounds_in_window: Option<u32>,
    pub next_actor: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub countdown: Option<Countdown>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_run: Option<ActiveRun>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instruction_queue: Option<Vec<InstructionQueueItem>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opencode_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kimi_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_sent: Option<HashMap<String, bool>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consecutive_failures: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<Failure>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_break: Option<BreakMarker>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub break_rejected_by: Option<BreakMarker>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_failure: Option<Failure>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health_check: Option<HealthCheckResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compact_retries: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_mode: Option<ExecutionMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue: Option<TaskQueueInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Countdown {
    pub status: String, // 'running' | 'paused' | 'elapsed' | 'skipped' | 'expired'
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remaining: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_actor: Option<String>,
    pub default_next_actor: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActiveRun {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    pub actor: String,
    pub started_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>, // 'running'
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id_before: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id_after: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskSettings {
    pub protocol_version: String,
    pub flow_policy: String,
    pub role_mode: String,
    pub launchers: HashMap<String, Launcher>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub implementer_actor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewer_actor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_consecutive_failures: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed_claude_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed_codex_thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed_cursor_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed_opencode_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed_kimi_session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Launcher {
    pub command: String,
    #[serde(default)]
    pub env: HashMap<String, String>,
    pub timeout_seconds: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptEntry {
    pub role: String, // 'human' | 'claude' | 'codex' | 'cursor' | 'opencode' | 'kimi' | 'system'
    pub content: String,
    pub ts: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub round: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<serde_json::Map<String, serde_json::Value>>,
    /// Per-line sequence from transcript.jsonl (read as
    /// `TranscriptEntry & { seq?: number }` by the TS store; not part of
    /// `shared/types.ts`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(rename = "type")]
    pub event_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    pub ts: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default)]
    pub payload: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Failure {
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ts: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_file: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthResponse {
    pub app: String,
    pub version: String,
    pub pid: u32,
    pub host: String,
    pub port: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BootstrapResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub repo_root: String,
    pub data_root: String,
    pub home_dir: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locale: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_key: Option<String>,
    pub tasks: Vec<Task>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub global_settings: Option<GlobalSettings>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GlobalSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub countdown_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_rounds: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_consecutive_failures: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launchers: Option<HashMap<String, Launcher>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed_claude_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed_codex_thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed_cursor_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed_opencode_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed_kimi_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_compact_retries: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_generate_commit_message: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_notifications_enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_upgrade_retries: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_prompt_implementer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_prompt_reviewer: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuddyError {
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recoverable: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestLauncherResult {
    pub actor: String,
    pub success: bool,
    pub phase: String, // 'tool_check' | 'ping'
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(rename = "sessionId", default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(rename = "threadId", default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    #[serde(rename = "responsePreview", default, skip_serializing_if = "Option::is_none")]
    pub response_preview: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskEventEnvelope {
    pub workspace_key: String,
    pub task_id: String,
    pub event: Event,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateTaskInput {
    pub task_id: String,
    #[serde(default)]
    pub repo_root: Option<String>,
    #[serde(default)]
    pub task_text: Option<String>,
    #[serde(default)]
    pub context_text: Option<String>,
    #[serde(default)]
    pub settings: Option<HashMap<String, serde_json::Value>>,
    #[serde(default)]
    pub execution_mode: Option<ExecutionMode>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateTaskResult {
    pub task: String,
    pub path: String,
    pub workspace_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct StartTaskInput {
    #[serde(default)]
    pub actor: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub workspace_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Attachment {
    pub id: String,
    pub name: String,
    pub category: String, // 'image' | 'file'
    pub mime_type: String,
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub buffer_base64: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentMeta {
    pub path: String,
    pub name: String,
    pub mime_type: String,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SendMessageInput {
    #[serde(default)]
    pub actor: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub workspace_key: Option<String>,
    #[serde(default)]
    pub attachments: Option<Vec<Attachment>>,
    #[serde(default, rename = "attachmentMeta")]
    pub attachment_meta: Option<Vec<AttachmentMeta>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CountdownInput {
    #[serde(default)]
    pub next_actor: Option<String>,
    #[serde(default)]
    pub workspace_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoundEventEntry {
    #[serde(rename = "type")]
    pub entry_type: String, // 'thinking' | 'text' | 'tool_use' | 'tool_result' | 'result'
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_length: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_input: Option<serde_json::Map<String, serde_json::Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_result_preview: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoundEventSummary {
    pub run_id: String,
    pub events: Vec<RoundEventEntry>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskActorStats {
    pub actor: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub duration_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    pub rounds: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskStats {
    pub actors: Vec<TaskActorStats>,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_cache_read_tokens: u64,
    pub total_duration_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_cost_usd: Option<f64>,
    pub total_rounds: u32,
}

pub type GitFileStatusCode = String; // 'M' | 'A' | 'D' | 'R' | 'C' | 'U' | '?'

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitFileStatus {
    pub path: String,
    pub status: GitFileStatusCode,
    pub insertions: u64,
    pub deletions: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitDiffStats {
    pub files_changed: u64,
    pub insertions: u64,
    pub deletions: u64,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files: Option<Vec<GitFileStatus>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitRemote {
    pub name: String,
    pub url: String,
}

/// 当前本地分支的默认跟踪目标 (remote + 目标分支), 不含 refs/heads/ 前缀。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GitUpstream {
    pub remote: String,
    pub branch: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitStatusResult {
    pub branch: String,
    pub diff: Option<GitDiffStats>,
    pub staged: Option<GitDiffStats>,
    pub untracked: u64,
    pub files: Vec<GitFileStatus>,
    pub remotes: Vec<GitRemote>,
    /// 当前分支的 Git upstream; 无当前分支、分离 HEAD 或未配置时为 null。
    pub upstream: Option<GitUpstream>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum GitPushStatus {
    #[serde(rename = "not_requested")]
    NotRequested,
    #[serde(rename = "pushed")]
    Pushed,
    #[serde(rename = "failed")]
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitCommitPushResult {
    pub commit_hash: String,
    pub push_status: GitPushStatus,
    pub remote: Option<String>,
    pub upstream_created: bool,
    pub push_error: Option<String>,
}

/// 独立「推送已有提交」入口的远端可推性状态（v1.2.20）。
/// fetch 本身失败不映射为某个 state, 而是让查询报错,
/// 由调用方呈现「检查远端状态失败」而非伪装成可推送或已同步。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum GitPushAvailabilityState {
    /// 本地领先, 远端未领先
    #[serde(rename = "ahead")]
    Ahead,
    /// 本地与远端同步
    #[serde(rename = "up_to_date")]
    UpToDate,
    /// 仅落后
    #[serde(rename = "behind")]
    Behind,
    /// 双方都有各自提交
    #[serde(rename = "diverged")]
    Diverged,
    /// 远端尚无目标分支, 首次推送
    #[serde(rename = "new_branch")]
    NewBranch,
    /// 无有效 HEAD / 分离 HEAD 等不可推送情形
    #[serde(rename = "unavailable")]
    Unavailable,
}

/// 一条尚未推送的本地提交: 7 位短 SHA 与完整标题 (subject)。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct GitPendingCommit {
    pub hash: String,
    pub subject: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitPushAvailability {
    pub state: GitPushAvailabilityState,
    pub remote: String,
    pub branch: String,
    pub ahead: u64,
    pub behind: u64,
    /// 仅 ahead 时为 `<remote-ref>..HEAD` 范围内、从旧到新的本地独有提交;
    /// 其余状态 (含 new_branch/同步/落后/分叉/不可用) 一律为 []。
    /// 解析失败时让 get_git_push_availability 抛错, 不返回部分列表。
    pub pending_commits: Vec<GitPendingCommit>,
    /// 本次推送是否会在成功后建立 upstream (仅无 upstream 的非分离 HEAD 首次推送为 true)。
    pub upstream_created_on_push: bool,
}

/// 独立推送结果: 只推当前 HEAD, 不产生新提交、不改动工作区。
/// push_status 只会是 Pushed / Failed (复用 GitPushStatus, wire 格式一致)。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitPushResult {
    pub push_status: GitPushStatus,
    pub remote: String,
    pub upstream_created: bool,
    pub push_error: Option<String>,
}
