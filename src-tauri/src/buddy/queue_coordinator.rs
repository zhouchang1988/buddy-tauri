//! Per-project FIFO queue coordinator, port of
//! `src/main/buddy/queue-coordinator.ts`.
//!
//! A queued task (`execution_mode == 'queued'`) belongs to exactly one
//! `workspace_key`. Within a workspace, queued tasks form a FIFO ordered by
//! `enqueued_at`, then `created_at`, then `task_id`. At most one queued task
//! may be "active" (running or paused/failed from a prior run) per workspace.
//!
//! Auto-advancement conditions (all must hold) before the earliest waiting
//! queued task starts:
//!  1. No incomplete immediate-execution task in the workspace blocks the queue.
//!  2. No queued task that is already active or blocking the queue
//!     (PAUSED, FAILED, PINGING, RUNNING, COUNTDOWN).
//!  3. The candidate is the earliest waiting queued task.
//!
//! A queued task only allows the next one to start after it reaches DONE.
//! PAUSED/FAILED blocks.
//!
//! Manual start (run now) of any queued task bypasses ordering/blockers: it
//! becomes the new active queue point, and every earlier non-DONE waiting
//! queued task is marked superseded (its data is preserved). After the manual
//! task reaches DONE, advancement resumes from the tasks created after it.
//!
//! Reconcile coalescing: each workspace keeps a single in-flight reconcile.
//! Extra requests arriving while one is running only set a `dirty` flag, so
//! at most one extra re-scan is appended after the current one finishes.
//! Different workspaces reconcile in parallel.

use crate::buddy::events::BuddyEventBus;
use crate::buddy::store::{BuddyStore, EventInput, StoreError};
use crate::buddy::types::{
    Event, ExecutionMode, Task, TaskEventEnvelope, TaskQueueInfo, TaskState, TaskStatus,
};
use async_trait::async_trait;
use chrono::Utc;
use parking_lot::Mutex;
use serde_json::{Map, Value};
use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

/// The slice of the runner the coordinator depends on. The real
/// implementation lives in `runner.rs` (integration wave); tests substitute a
/// mock. Mirrors `BuddyRunner.startTask(taskId, { workspace_key })` — the
/// `Err` string is the error message surfaced as a `queue.blocked` event.
#[async_trait]
pub trait QueueTaskRunner: Send + Sync {
    async fn start_task(&self, task_id: &str, workspace_key: &str) -> Result<(), String>;
}

#[derive(Debug, thiserror::Error)]
pub enum CoordinatorError {
    #[error("store error: {0}")]
    Store(#[from] StoreError),
    #[error("{0}")]
    Invalid(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActivationSource {
    Automatic,
    Manual,
}

impl ActivationSource {
    fn as_str(&self) -> &'static str {
        match self {
            ActivationSource::Automatic => "automatic",
            ActivationSource::Manual => "manual",
        }
    }
}

/// Signature identifying a unique blocked state, used to dedupe
/// `queue.blocked` events.
#[derive(Debug, Clone, PartialEq, Eq)]
struct BlockSignature {
    workspace_key: String,
    head_task_id: String,
    blocked_task_id: String,
    reason: String,
}

/// Per-workspace reconcile state. `running` marks an in-flight chain; `dirty`
/// means a later request arrived during the run and one re-scan must follow.
#[derive(Default)]
struct ReconcileFlags {
    running: bool,
    dirty: bool,
}

#[derive(Default)]
struct WorkspaceEntry {
    flags: Mutex<ReconcileFlags>,
    /// Wakes callers that folded their request into the in-flight chain.
    notify: tokio::sync::Notify,
}

struct QueueEntry {
    task: Task,
    state: TaskState,
}

/// Per-project FIFO queue coordinator. Cheap to clone (all state is shared).
#[derive(Clone)]
pub struct QueueCoordinator {
    store: Arc<BuddyStore>,
    runner: Arc<dyn QueueTaskRunner>,
    events: Option<BuddyEventBus>,
    reconcile_state: Arc<Mutex<HashMap<String, Arc<WorkspaceEntry>>>>,
    /// Most recent `queue.blocked` signature per workspace, so identical
    /// re-blocks don't flood the log.
    last_signature: Arc<Mutex<HashMap<String, BlockSignature>>>,
}

impl QueueCoordinator {
    pub fn new(store: Arc<BuddyStore>, runner: Arc<dyn QueueTaskRunner>) -> Self {
        QueueCoordinator {
            store,
            runner,
            events: None,
            reconcile_state: Arc::new(Mutex::new(HashMap::new())),
            last_signature: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn with_events(mut self, events: BuddyEventBus) -> Self {
        self.events = Some(events);
        self
    }

    /// Rebuild the per-workspace queue snapshots purely from disk state and
    /// run a single safe scheduling pass for every workspace. Called once on
    /// app startup, after recovery.
    pub async fn rebuild_and_reconcile_all(&self) -> Result<(), CoordinatorError> {
        let tasks = self.store.get_tasks().await;
        let workspace_keys: BTreeSet<String> =
            tasks.iter().map(|task| task.workspace_key.clone()).collect();
        let mut set = tokio::task::JoinSet::new();
        for workspace_key in workspace_keys {
            let coordinator = self.clone();
            set.spawn(async move { coordinator.reconcile(&workspace_key).await });
        }
        while let Some(result) = set.join_next().await {
            result.map_err(|error| CoordinatorError::Invalid(format!("reconcile task: {error}")))??;
        }
        Ok(())
    }

    /// Main entry point. Safe to call repeatedly and concurrently —
    /// per-workspace serialization guarantees the same waiting task is
    /// started at most once, and coalescing collapses bursts of redundant
    /// reconcile requests into at most one extra re-scan.
    ///
    /// Deviation from the Electron edition: callers that fold into an
    /// in-flight chain do not observe that chain's error; they resolve once
    /// the chain settles (the TS edition shares the same promise, error
    /// included).
    pub async fn reconcile(&self, workspace_key: &str) -> Result<(), CoordinatorError> {
        let (entry, am_runner) = {
            let mut map = self.reconcile_state.lock();
            let entry = map
                .entry(workspace_key.to_string())
                .or_insert_with(|| Arc::new(WorkspaceEntry::default()))
                .clone();
            let am_runner = {
                let mut flags = entry.flags.lock();
                if flags.running {
                    // A reconcile is already running for this workspace — fold
                    // this request into a single follow-up scan instead of
                    // queuing another full pass.
                    flags.dirty = true;
                    false
                } else {
                    flags.running = true;
                    true
                }
            };
            (entry, am_runner)
        };

        if !am_runner {
            // Wait until the in-flight chain fully settles.
            loop {
                let notified = entry.notify.notified();
                if !entry.flags.lock().running {
                    return Ok(());
                }
                notified.await;
            }
        }

        // Run reconcileInner, then if a later request marked the workspace
        // dirty during the run, re-scan once. Repeats only while new requests
        // keep arriving mid-scan. Each scan reads the latest disk state, so
        // coalescing never starts a task twice or acts on stale data.
        let mut result = Ok(());
        loop {
            if let Err(error) = self.reconcile_inner(workspace_key).await {
                result = Err(error);
            }
            let mut map = self.reconcile_state.lock();
            let mut flags = entry.flags.lock();
            if result.is_ok() && flags.dirty {
                // Clear dirty while still holding the entry so a request
                // arriving during the re-scan re-sets it instead of starting
                // a competing fresh chain.
                flags.dirty = false;
                continue;
            }
            flags.running = false;
            // The workspace entry is deleted once the chain fully settles, so
            // a request arriving after the last scan starts a clean chain.
            if map
                .get(workspace_key)
                .is_some_and(|current| Arc::ptr_eq(current, &entry))
            {
                map.remove(workspace_key);
            }
            drop(flags);
            drop(map);
            entry.notify.notify_waiters();
            break;
        }
        result
    }

    async fn reconcile_inner(&self, workspace_key: &str) -> Result<(), CoordinatorError> {
        let tasks = self.store.get_tasks().await;
        let workspace_tasks: Vec<&Task> = tasks
            .iter()
            .filter(|task| task.workspace_key == workspace_key)
            .collect();
        if workspace_tasks.is_empty() {
            self.last_signature.lock().remove(workspace_key);
            return Ok(());
        }

        // Load full states for queued/immediate tasks in this workspace.
        let mut states: Vec<QueueEntry> = Vec::new();
        for task in workspace_tasks {
            match self.store.read_task_state(&task.task_id, workspace_key).await {
                Ok(state) => states.push(QueueEntry {
                    task: task.clone(),
                    state,
                }),
                // Unreadable task — skip; detail load surfaces schema errors
                // elsewhere.
                Err(_) => {}
            }
        }

        // 1) Incomplete immediate-execution tasks block the queue.
        let has_incomplete_immediate = states.iter().any(|entry| {
            effective_mode(&entry.state) == ExecutionMode::Immediate && blocks_queue(&entry.state)
        });

        let queued_entries: Vec<&QueueEntry> = states
            .iter()
            .filter(|entry| effective_mode(&entry.state) == ExecutionMode::Queued)
            .collect();
        // 2) A queued task that is active (running/paused/failed/countdown/
        //    pinging) blocks advancement. Superseded tasks
        //    (queue.state == 'superseded') never block, even if not DONE.
        let has_active_queued = queued_entries.iter().any(|entry| {
            entry.state.queue.as_ref().map(|q| q.state.as_str()) != Some("superseded")
                && entry.state.status != TaskStatus::Queued
                && entry.state.status != TaskStatus::Done
        });

        if has_incomplete_immediate || has_active_queued {
            // Record a blocked event (deduped by signature) when a waiting
            // task exists but can't start.
            let blocker = find_blocker(&states, has_incomplete_immediate, has_active_queued);
            if let (Some(earliest), Some(blocker)) = (earliest_waiting(&queued_entries), blocker) {
                self.record_blocked(workspace_key, earliest, &blocker).await?;
            }
            return Ok(());
        }

        // 3) Pick the earliest waiting queued task.
        let Some(candidate) = earliest_waiting(&queued_entries) else {
            // Queue successfully idle/advanced — clear any prior blocked
            // signature.
            self.last_signature.lock().remove(workspace_key);
            return Ok(());
        };

        // About to advance — clear the prior blocked signature before
        // starting the next task.
        self.last_signature.lock().remove(workspace_key);
        self.activate_and_start(workspace_key, candidate, ActivationSource::Automatic)
            .await
    }

    /// Activate a waiting queued task (mark queue.state=active) and start it.
    /// Used by both automatic advancement and manual "run now".
    async fn activate_and_start(
        &self,
        workspace_key: &str,
        entry: &QueueEntry,
        source: ActivationSource,
    ) -> Result<(), CoordinatorError> {
        let now = utc_now();
        let task_id = entry.task.task_id.clone();
        // Atomically flip queue.state to active + status to READY so the
        // runner can pick it up.
        let activate_now = now.clone();
        let source_str = source.as_str().to_string();
        self.store
            .update_task_state(&task_id, workspace_key, move |mut state| {
                state.status = TaskStatus::Ready;
                let mut queue = state.queue.take().unwrap_or(TaskQueueInfo {
                    state: "waiting".to_string(),
                    enqueued_at: activate_now.clone(),
                    activated_at: None,
                    activation_source: None,
                });
                queue.state = "active".to_string();
                queue.activated_at = Some(activate_now.clone());
                queue.activation_source = Some(source_str.clone());
                state.queue = Some(queue);
                state
            })
            .await?;

        let mut payload = Map::new();
        payload.insert(
            "activation_source".to_string(),
            Value::String(source.as_str().to_string()),
        );
        payload.insert(
            "enqueued_at".to_string(),
            Value::String(
                entry
                    .state
                    .queue
                    .as_ref()
                    .map(|queue| queue.enqueued_at.clone())
                    .unwrap_or_else(|| now.clone()),
            ),
        );
        self.append_queue_event(workspace_key, &task_id, "queue.activated", payload)
            .await?;

        // Start the task. If the runner throws (e.g. round window), it leaves
        // the task PAUSED which blocks the queue — which is the desired
        // behavior.
        if let Err(message) = self.runner.start_task(&task_id, workspace_key).await {
            // Activation already recorded; the runner transitioned to
            // PAUSED/FAILED as appropriate. Surface the failure as a queue
            // event for observability.
            let mut payload = Map::new();
            payload.insert("reason".to_string(), Value::String("start_failed".to_string()));
            payload.insert(
                "blocked_task_id".to_string(),
                Value::String(task_id.clone()),
            );
            payload.insert(
                "error".to_string(),
                Value::String(message.chars().take(300).collect()),
            );
            self.append_queue_event(workspace_key, &task_id, "queue.blocked", payload)
                .await?;
        }
        Ok(())
    }

    /// Manual run now: activate a queued task out of order. Every earlier
    /// non-DONE queued task is superseded (state preserved, removed from the
    /// auto-advancement chain) — whether it was waiting (QUEUED) or already
    /// active but blocked (PAUSED/FAILED/COUNTDOWN). This matches the spec:
    /// earlier non-completed queued tasks leave the auto-advancement chain on
    /// manual start.
    ///
    /// Bypasses immediate-task blockers and queue ordering. After this task
    /// reaches DONE the queue advances from the tasks created after it.
    pub async fn start_queued_now(
        &self,
        task_id: &str,
        workspace_key: &str,
    ) -> Result<(), CoordinatorError> {
        let state = self.store.read_task_state(task_id, workspace_key).await?;
        if effective_mode(&state) != ExecutionMode::Queued {
            return Err(CoordinatorError::Invalid(
                "Task is not a queued task".to_string(),
            ));
        }
        // Supersede every earlier non-DONE queued task (waiting OR
        // active-but-blocked).
        let tasks = self.store.get_tasks().await;
        for task in tasks.iter().filter(|t| t.workspace_key == workspace_key) {
            if task.task_id == task_id {
                continue;
            }
            let Ok(other) = self.store.read_task_state(&task.task_id, workspace_key).await else {
                continue; // Skip unreadable.
            };
            if effective_mode(&other) != ExecutionMode::Queued {
                continue;
            }
            // Only supersede tasks created earlier than the manually-started
            // one.
            if compare_queue_order(&other, &task.task_id, &state, task_id) != Ordering::Less {
                continue;
            }
            // Skip tasks already DONE (nothing to do) or already superseded
            // (idempotent).
            if other.status == TaskStatus::Done {
                continue;
            }
            if other.queue.as_ref().map(|q| q.state.as_str()) == Some("superseded") {
                continue;
            }
            self.store
                .update_task_state(&task.task_id, workspace_key, |mut st| {
                    let mut queue = st.queue.take().unwrap_or(TaskQueueInfo {
                        state: "waiting".to_string(),
                        enqueued_at: utc_now(),
                        activated_at: None,
                        activation_source: None,
                    });
                    queue.state = "superseded".to_string();
                    st.queue = Some(queue);
                    st
                })
                .await?;
            let mut payload = Map::new();
            payload.insert(
                "superseded_by".to_string(),
                Value::String(task_id.to_string()),
            );
            if let Some(queue) = &other.queue {
                payload.insert(
                    "original_enqueued_at".to_string(),
                    Value::String(queue.enqueued_at.clone()),
                );
                payload.insert(
                    "prior_queue_state".to_string(),
                    Value::String(queue.state.clone()),
                );
            }
            payload.insert(
                "prior_status".to_string(),
                Value::String(other.status.as_str().to_string()),
            );
            self.append_queue_event(workspace_key, &task.task_id, "queue.superseded", payload)
                .await?;
        }

        // Activate the chosen task manually. If it was previously waiting (or
        // superseded), flip to active+READY. If it was PAUSED/FAILED/
        // COUNTDOWN (an active task being manually resumed), keep its queue
        // identity active and resume execution.
        let queue_state = state.queue.as_ref().map(|q| q.state.as_str());
        let is_queued_waiting = state.status == TaskStatus::Queued
            && (queue_state == Some("waiting") || queue_state == Some("superseded"));
        if is_queued_waiting {
            let entry = QueueEntry {
                task: Task {
                    task_id: task_id.to_string(),
                    workspace_key: workspace_key.to_string(),
                    status: state.status.clone(),
                    updated_at: String::new(),
                    repo_root: String::new(),
                    round: None,
                    active_run: None,
                    execution_mode: state.execution_mode,
                    queue: state.queue.clone(),
                    created_at: state.created_at.clone(),
                },
                state,
            };
            self.activate_and_start(workspace_key, &entry, ActivationSource::Manual)
                .await
        } else {
            // Already active (paused/failed/countdown) — manual resume keeps
            // queue identity.
            let now = utc_now();
            let resume_now = now.clone();
            self.store
                .update_task_state(task_id, workspace_key, move |mut st| {
                    // If somehow still QUEUED, flip to READY so the runner
                    // can start it.
                    if st.status == TaskStatus::Queued {
                        st.status = TaskStatus::Ready;
                    }
                    let mut queue = st.queue.take().unwrap_or(TaskQueueInfo {
                        state: "active".to_string(),
                        enqueued_at: resume_now.clone(),
                        activated_at: None,
                        activation_source: None,
                    });
                    queue.state = "active".to_string();
                    queue.activation_source = Some("manual".to_string());
                    if queue.activated_at.is_none() {
                        queue.activated_at = Some(resume_now.clone());
                    }
                    st.queue = Some(queue);
                    st
                })
                .await?;
            let mut payload = Map::new();
            payload.insert(
                "activation_source".to_string(),
                Value::String("manual".to_string()),
            );
            payload.insert(
                "enqueued_at".to_string(),
                Value::String(
                    state
                        .queue
                        .as_ref()
                        .map(|queue| queue.enqueued_at.clone())
                        .unwrap_or_else(|| now.clone()),
                ),
            );
            self.append_queue_event(workspace_key, task_id, "queue.activated", payload)
                .await?;
            // Runner sets PAUSED/FAILED on error; reconcile will block,
            // which is correct.
            let _ = self.runner.start_task(task_id, workspace_key).await;
            Ok(())
        }
    }

    /// Called after a task transitions to DONE/PAUSED/FAILED to advance the
    /// workspace queue.
    pub async fn on_task_terminal(&self, workspace_key: &str) -> Result<(), CoordinatorError> {
        self.reconcile(workspace_key).await
    }

    async fn record_blocked(
        &self,
        workspace_key: &str,
        entry: &QueueEntry,
        blocker: &Blocker,
    ) -> Result<(), CoordinatorError> {
        let signature = BlockSignature {
            workspace_key: workspace_key.to_string(),
            head_task_id: entry.task.task_id.clone(),
            blocked_task_id: blocker.task_id.clone(),
            reason: blocker.reason.clone(),
        };
        // Dedupe: only emit a new queue.blocked when the blocked signature
        // actually changes.
        {
            let mut signatures = self.last_signature.lock();
            if signatures.get(workspace_key) == Some(&signature) {
                return Ok(());
            }
            signatures.insert(workspace_key.to_string(), signature);
        }
        let mut payload = Map::new();
        payload.insert("reason".to_string(), Value::String(blocker.reason.clone()));
        payload.insert(
            "blocked_task_id".to_string(),
            Value::String(blocker.task_id.clone()),
        );
        if let Some(queue) = &entry.state.queue {
            payload.insert(
                "enqueued_at".to_string(),
                Value::String(queue.enqueued_at.clone()),
            );
        }
        self.append_queue_event(workspace_key, &entry.task.task_id, "queue.blocked", payload)
            .await?;
        Ok(())
    }

    async fn append_queue_event(
        &self,
        workspace_key: &str,
        task_id: &str,
        event_type: &str,
        payload: Map<String, Value>,
    ) -> Result<Event, CoordinatorError> {
        // Attach to the task's event log so it flows through redaction and
        // the renderer event view.
        let mut full_payload = Map::new();
        full_payload.insert(
            "workspace_key".to_string(),
            Value::String(workspace_key.to_string()),
        );
        full_payload.insert("task_id".to_string(), Value::String(task_id.to_string()));
        full_payload.extend(payload);
        let event = self
            .store
            .append_task_event(
                task_id,
                workspace_key,
                EventInput {
                    event_type: event_type.to_string(),
                    payload: full_payload,
                    ..Default::default()
                },
            )
            .await?;
        if let Some(events) = &self.events {
            events.publish(TaskEventEnvelope {
                workspace_key: workspace_key.to_string(),
                task_id: task_id.to_string(),
                event: event.clone(),
            });
        }
        Ok(event)
    }
}

struct Blocker {
    task_id: String,
    reason: String,
}

fn find_blocker(
    states: &[QueueEntry],
    has_incomplete_immediate: bool,
    has_active_queued: bool,
) -> Option<Blocker> {
    if has_active_queued {
        let active = states.iter().find(|entry| {
            effective_mode(&entry.state) == ExecutionMode::Queued
                && entry.state.queue.as_ref().map(|q| q.state.as_str()) != Some("superseded")
                && entry.state.status != TaskStatus::Done
                && entry.state.status != TaskStatus::Queued
        });
        if let Some(active) = active {
            return Some(Blocker {
                task_id: active.task.task_id.clone(),
                reason: "active_queued_task".to_string(),
            });
        }
    }
    if has_incomplete_immediate {
        let immediate = states.iter().find(|entry| {
            effective_mode(&entry.state) == ExecutionMode::Immediate && blocks_queue(&entry.state)
        });
        if let Some(immediate) = immediate {
            return Some(Blocker {
                task_id: immediate.task.task_id.clone(),
                reason: "incomplete_immediate_task".to_string(),
            });
        }
    }
    None
}

fn earliest_waiting<'a>(entries: &[&'a QueueEntry]) -> Option<&'a QueueEntry> {
    entries
        .iter()
        .copied()
        .filter(|entry| {
            entry.state.status == TaskStatus::Queued
                && entry.state.queue.as_ref().map(|q| q.state.as_str()) == Some("waiting")
        })
        .min_by(|a, b| compare_queue_order(&a.state, &a.task.task_id, &b.state, &b.task.task_id))
}

/// Effective execution mode. Legacy tasks without the field default to
/// immediate.
fn effective_mode(state: &TaskState) -> ExecutionMode {
    state.execution_mode.unwrap_or(ExecutionMode::Immediate)
}

/// Whether a task in a given state blocks the queue's auto-advancement.
///
/// Two cases differ by execution_mode:
/// - Explicit immediate (`execution_mode == 'immediate'`): any non-DONE state
///   blocks, matching the contract that an incomplete immediate task prevents
///   queued advancement.
/// - Legacy tasks (`execution_mode == None`, pre-queue feature): only
///   actively-running states block. A leftover READY/PAUSED/FAILED legacy
///   task must NOT permanently block new queued tasks, since the user never
///   opted that task into the queue discipline.
///
/// Explicit queued tasks are handled by the has_active_queued branch, not
/// this helper.
fn blocks_queue(state: &TaskState) -> bool {
    match state.execution_mode {
        None => is_actively_running(&state.status),
        Some(_) => state.status != TaskStatus::Done,
    }
}

/// States where a task is genuinely executing or mid-round, and so genuinely
/// holds the queue.
fn is_actively_running(status: &TaskStatus) -> bool {
    matches!(
        status,
        TaskStatus::Pinging
            | TaskStatus::RunningClaude
            | TaskStatus::RunningCodex
            | TaskStatus::RunningCursor
            | TaskStatus::RunningOpencode
            | TaskStatus::RunningKimi
            | TaskStatus::Countdown
    )
}

/// Stable ordering for queued tasks: enqueued_at, then created_at, then
/// task_id. Lower comes first.
fn compare_queue_order(
    a_state: &TaskState,
    a_task_id: &str,
    b_state: &TaskState,
    b_task_id: &str,
) -> Ordering {
    let a_enq = a_state
        .queue
        .as_ref()
        .map(|queue| queue.enqueued_at.as_str())
        .or(a_state.created_at.as_deref())
        .unwrap_or("");
    let b_enq = b_state
        .queue
        .as_ref()
        .map(|queue| queue.enqueued_at.as_str())
        .or(b_state.created_at.as_deref())
        .unwrap_or("");
    if a_enq != b_enq {
        return a_enq.cmp(b_enq);
    }
    let a_created = a_state.created_at.as_deref().unwrap_or("");
    let b_created = b_state.created_at.as_deref().unwrap_or("");
    if a_created != b_created {
        return a_created.cmp(b_created);
    }
    a_task_id.cmp(b_task_id)
}

/// `new Date().toISOString().replace(/\.\d{3}Z$/, 'Z')` — second precision.
fn utc_now() -> String {
    Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buddy::types::{ActiveRun, CreateTaskInput};
    use std::path::Path;

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct StartCall {
        task_id: String,
        workspace_key: String,
    }

    /// Mirrors the TS test mock: records the call and simulates a successful
    /// actor start by moving the task to RUNNING_CLAUDE (active).
    struct MockRunner {
        store: Arc<BuddyStore>,
        calls: Mutex<Vec<StartCall>>,
    }

    #[async_trait]
    impl QueueTaskRunner for MockRunner {
        async fn start_task(&self, task_id: &str, workspace_key: &str) -> Result<(), String> {
            self.calls.lock().push(StartCall {
                task_id: task_id.to_string(),
                workspace_key: workspace_key.to_string(),
            });
            self.store
                .update_task_state(task_id, workspace_key, |mut state| {
                    state.status = TaskStatus::RunningClaude;
                    state.active_run = Some(ActiveRun {
                        run_id: None,
                        actor: "claude".to_string(),
                        started_at: utc_now(),
                        status: None,
                        session_id_before: None,
                        session_id_after: None,
                    });
                    state
                })
                .await
                .map_err(|error| error.to_string())?;
            Ok(())
        }
    }

    fn make_coordinator(root: &Path) -> (Arc<BuddyStore>, Arc<MockRunner>, QueueCoordinator) {
        let store = Arc::new(BuddyStore::new(root));
        let runner = Arc::new(MockRunner {
            store: store.clone(),
            calls: Mutex::new(Vec::new()),
        });
        let coordinator = QueueCoordinator::new(store.clone(), runner.clone());
        (store, runner, coordinator)
    }

    fn start_calls(runner: &MockRunner) -> Vec<String> {
        runner
            .calls
            .lock()
            .iter()
            .map(|call| call.task_id.clone())
            .collect()
    }

    async fn create_queued(
        store: &BuddyStore,
        id: &str,
        repo: &str,
        enqueued_at: Option<&str>,
    ) -> String {
        let created = store
            .create_task(CreateTaskInput {
                task_id: id.to_string(),
                repo_root: Some(repo.to_string()),
                task_text: None,
                context_text: None,
                settings: None,
                execution_mode: Some(ExecutionMode::Queued),
            })
            .await
            .unwrap();
        if let Some(enqueued_at) = enqueued_at {
            let enqueued_at = enqueued_at.to_string();
            store
                .update_task_state(id, &created.workspace_key, move |mut state| {
                    if let Some(mut queue) = state.queue.take() {
                        queue.enqueued_at = enqueued_at.clone();
                        state.queue = Some(queue);
                    }
                    state.created_at = Some(enqueued_at.clone());
                    state
                })
                .await
                .unwrap();
        }
        created.workspace_key
    }

    async fn create_immediate(store: &BuddyStore, id: &str, repo: &str) -> String {
        store
            .create_task(CreateTaskInput {
                task_id: id.to_string(),
                repo_root: Some(repo.to_string()),
                task_text: None,
                context_text: None,
                settings: None,
                execution_mode: None,
            })
            .await
            .unwrap()
            .workspace_key
    }

    async fn set_status(store: &BuddyStore, ws: &str, id: &str, status: TaskStatus) {
        store
            .update_task_state(id, ws, move |mut state| {
                state.status = status;
                state
            })
            .await
            .unwrap();
    }

    /// Sets a terminal/idle status and clears active_run (the TS tests do
    /// `{ ...s, status, active_run: null }`).
    async fn settle_status(store: &BuddyStore, ws: &str, id: &str, status: TaskStatus) {
        store
            .update_task_state(id, ws, move |mut state| {
                state.status = status;
                state.active_run = None;
                state
            })
            .await
            .unwrap();
    }

    /// Overwrite state.json with a minimal legacy shape (no execution_mode /
    /// queue fields), as written by pre-queue versions of the app.
    fn write_legacy_state(store: &BuddyStore, id: &str, ws: &str, status: &str) {
        let value = serde_json::json!({
            "protocol_version": "1",
            "task_id": id,
            "repo_root": "/tmp/repo",
            "status": status,
            "round": 1,
            "next_actor": "claude",
            "active_run": null,
            "instruction_queue": []
        });
        std::fs::write(
            store.task_directory(id, ws).join("state.json"),
            serde_json::to_string(&value).unwrap(),
        )
        .unwrap();
    }

    fn blocked_events(events: &[Event]) -> Vec<&Event> {
        events
            .iter()
            .filter(|event| event.event_type == "queue.blocked")
            .collect()
    }

    #[tokio::test]
    async fn creates_queued_task_waiting_and_default_mode_is_immediate() {
        let dir = tempfile::tempdir().unwrap();
        let (store, _runner, _coordinator) = make_coordinator(dir.path());
        let ws = create_queued(&store, "q1", "/tmp/repo", None).await;
        let state = store.read_task_state("q1", &ws).await.unwrap();
        assert_eq!(state.status, TaskStatus::Queued);
        assert_eq!(state.execution_mode, Some(ExecutionMode::Queued));
        assert_eq!(state.queue.as_ref().map(|q| q.state.as_str()), Some("waiting"));

        let imm_ws = create_immediate(&store, "i1", "/tmp/repo").await;
        let imm_state = store.read_task_state("i1", &imm_ws).await.unwrap();
        assert_eq!(imm_state.execution_mode, Some(ExecutionMode::Immediate));
        assert_eq!(imm_state.status, TaskStatus::Ready);
    }

    #[tokio::test]
    async fn auto_starts_earliest_waiting_queued_task_when_nothing_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let (store, runner, coordinator) = make_coordinator(dir.path());
        let ws = create_queued(&store, "q1", "/tmp/repo", None).await;
        coordinator.reconcile(&ws).await.unwrap();
        assert_eq!(start_calls(&runner), vec!["q1"]);
        let state = store.read_task_state("q1", &ws).await.unwrap();
        assert_eq!(state.queue.as_ref().map(|q| q.state.as_str()), Some("active"));
        assert_eq!(
            state.queue.as_ref().and_then(|q| q.activation_source.as_deref()),
            Some("automatic")
        );
        assert_eq!(state.status, TaskStatus::RunningClaude);
    }

    #[tokio::test]
    async fn does_not_start_runner_without_reconcile() {
        let dir = tempfile::tempdir().unwrap();
        let (store, runner, _coordinator) = make_coordinator(dir.path());
        let ws = create_queued(&store, "q1", "/tmp/repo", None).await;
        // No reconcile call — waiting task must not touch the launcher.
        assert!(start_calls(&runner).is_empty());
        let state = store.read_task_state("q1", &ws).await.unwrap();
        assert_eq!(state.status, TaskStatus::Queued);
    }

    #[tokio::test]
    async fn runs_queued_tasks_in_creation_order() {
        let dir = tempfile::tempdir().unwrap();
        let (store, runner, coordinator) = make_coordinator(dir.path());
        // Distinct enqueued_at timestamps guarantee ordering.
        let ws_a = create_queued(&store, "a", "/tmp/repo", Some("2026-01-01T00:00:01Z")).await;
        let ws_b = create_queued(&store, "b", "/tmp/repo", Some("2026-01-01T00:00:02Z")).await;
        assert_eq!(ws_a, ws_b);
        let ws = ws_a;
        coordinator.reconcile(&ws).await.unwrap();
        assert_eq!(start_calls(&runner), vec!["a"]);
        // Mark a DONE, then reconcile → b starts.
        settle_status(&store, &ws, "a", TaskStatus::Done).await;
        coordinator.reconcile(&ws).await.unwrap();
        assert_eq!(start_calls(&runner), vec!["a", "b"]);
    }

    #[tokio::test]
    async fn keeps_different_project_queues_independent() {
        let dir = tempfile::tempdir().unwrap();
        let (store, runner, coordinator) = make_coordinator(dir.path());
        let ws1 = create_queued(&store, "p1", "/tmp/repo1", None).await;
        let ws2 = create_queued(&store, "p2", "/tmp/repo2", None).await;
        assert_ne!(ws1, ws2);
        coordinator.reconcile(&ws1).await.unwrap();
        coordinator.reconcile(&ws2).await.unwrap();
        let mut calls = start_calls(&runner);
        calls.sort();
        assert_eq!(calls, vec!["p1", "p2"]);
    }

    #[tokio::test]
    async fn blocks_queue_start_while_incomplete_immediate_task_exists() {
        let dir = tempfile::tempdir().unwrap();
        let (store, runner, coordinator) = make_coordinator(dir.path());
        let imm_ws = create_immediate(&store, "imm", "/tmp/repo").await;
        // immediate task running
        set_status(&store, &imm_ws, "imm", TaskStatus::RunningClaude).await;
        let ws = create_queued(&store, "q1", "/tmp/repo", None).await;
        assert_eq!(ws, imm_ws);
        coordinator.reconcile(&ws).await.unwrap();
        assert!(start_calls(&runner).is_empty());
        let state = store.read_task_state("q1", &ws).await.unwrap();
        assert_eq!(state.status, TaskStatus::Queued);
    }

    #[tokio::test]
    async fn later_immediate_task_starts_immediately_and_blocks_next_queued() {
        let dir = tempfile::tempdir().unwrap();
        let (store, runner, coordinator) = make_coordinator(dir.path());
        let ws = create_queued(&store, "q1", "/tmp/repo", None).await;
        // An immediate task created afterwards.
        let imm_ws = create_immediate(&store, "imm", "/tmp/repo").await;
        // Start the immediate task via the runner directly (renderer would
        // call startTask).
        runner.start_task("imm", &imm_ws).await.unwrap();
        assert!(start_calls(&runner).contains(&"imm".to_string()));
        // Reconcile should NOT start the queued task because imm is incomplete.
        coordinator.reconcile(&ws).await.unwrap();
        assert!(!start_calls(&runner).contains(&"q1".to_string()));
    }

    #[tokio::test]
    async fn paused_queued_task_blocks_subsequent_queued_advancement() {
        let dir = tempfile::tempdir().unwrap();
        let (store, runner, coordinator) = make_coordinator(dir.path());
        let ws = create_queued(&store, "a", "/tmp/repo", Some("2026-01-01T00:00:01Z")).await;
        create_queued(&store, "b", "/tmp/repo", Some("2026-01-01T00:00:02Z")).await;
        // Start a, then force it into PAUSED (active).
        coordinator.reconcile(&ws).await.unwrap();
        settle_status(&store, &ws, "a", TaskStatus::Paused).await;
        coordinator.reconcile(&ws).await.unwrap();
        // b must not start while a is PAUSED.
        assert_eq!(start_calls(&runner), vec!["a"]);
    }

    #[tokio::test]
    async fn manual_start_queued_now_bypasses_blockers_and_ordering() {
        let dir = tempfile::tempdir().unwrap();
        let (store, runner, coordinator) = make_coordinator(dir.path());
        let ws = create_queued(&store, "a", "/tmp/repo", Some("2026-01-01T00:00:01Z")).await;
        create_queued(&store, "b", "/tmp/repo", Some("2026-01-01T00:00:02Z")).await;
        // An incomplete immediate task exists (would normally block).
        let imm_ws = create_immediate(&store, "imm", "/tmp/repo").await;
        set_status(&store, &imm_ws, "imm", TaskStatus::RunningClaude).await;
        // Manually start b.
        coordinator.start_queued_now("b", &ws).await.unwrap();
        assert_eq!(start_calls(&runner), vec!["b"]);
        // a is superseded.
        let a_state = store.read_task_state("a", &ws).await.unwrap();
        assert_eq!(a_state.queue.as_ref().map(|q| q.state.as_str()), Some("superseded"));
        assert_eq!(a_state.status, TaskStatus::Queued);
    }

    #[tokio::test]
    async fn after_manual_start_earlier_tasks_no_longer_block_advancement() {
        let dir = tempfile::tempdir().unwrap();
        let (store, runner, coordinator) = make_coordinator(dir.path());
        let ws = create_queued(&store, "a", "/tmp/repo", Some("2026-01-01T00:00:01Z")).await;
        create_queued(&store, "b", "/tmp/repo", Some("2026-01-01T00:00:02Z")).await;
        create_queued(&store, "c", "/tmp/repo", Some("2026-01-01T00:00:03Z")).await;
        // Manually start b.
        coordinator.start_queued_now("b", &ws).await.unwrap();
        // b completes.
        settle_status(&store, &ws, "b", TaskStatus::Done).await;
        // Reconcile: a is superseded (not waiting), c should start.
        coordinator.reconcile(&ws).await.unwrap();
        assert_eq!(start_calls(&runner), vec!["b", "c"]);
        let a_state = store.read_task_state("a", &ws).await.unwrap();
        assert_eq!(a_state.queue.as_ref().map(|q| q.state.as_str()), Some("superseded"));
    }

    #[tokio::test]
    async fn manual_start_supersedes_earlier_paused_queued_task() {
        let dir = tempfile::tempdir().unwrap();
        let (store, runner, coordinator) = make_coordinator(dir.path());
        let ws = create_queued(&store, "a", "/tmp/repo", Some("2026-01-01T00:00:01Z")).await;
        create_queued(&store, "b", "/tmp/repo", Some("2026-01-01T00:00:02Z")).await;
        create_queued(&store, "c", "/tmp/repo", Some("2026-01-01T00:00:03Z")).await;
        // a started then paused (active, blocked).
        coordinator.reconcile(&ws).await.unwrap();
        settle_status(&store, &ws, "a", TaskStatus::Paused).await;
        // Reconcile must not start b or c while a is PAUSED.
        coordinator.reconcile(&ws).await.unwrap();
        assert_eq!(start_calls(&runner), vec!["a"]);
        // Manually start c — a (PAUSED) must be superseded, not continue
        // blocking. b is earlier than c, so b is superseded too.
        coordinator.start_queued_now("c", &ws).await.unwrap();
        assert_eq!(start_calls(&runner), vec!["a", "c"]);
        let a_state = store.read_task_state("a", &ws).await.unwrap();
        assert_eq!(a_state.queue.as_ref().map(|q| q.state.as_str()), Some("superseded"));
        let b_state = store.read_task_state("b", &ws).await.unwrap();
        assert_eq!(b_state.queue.as_ref().map(|q| q.state.as_str()), Some("superseded"));
        settle_status(&store, &ws, "c", TaskStatus::Done).await;
        coordinator.reconcile(&ws).await.unwrap();
        // No further auto-start: a and b are superseded, nothing waiting
        // after c.
        assert_eq!(start_calls(&runner), vec!["a", "c"]);
    }

    #[tokio::test]
    async fn can_manually_start_a_superseded_queued_task() {
        let dir = tempfile::tempdir().unwrap();
        let (store, runner, coordinator) = make_coordinator(dir.path());
        let ws = create_queued(&store, "a", "/tmp/repo", Some("2026-01-01T00:00:01Z")).await;
        create_queued(&store, "b", "/tmp/repo", Some("2026-01-01T00:00:02Z")).await;
        // Manually start b → a becomes superseded.
        coordinator.start_queued_now("b", &ws).await.unwrap();
        let a_state = store.read_task_state("a", &ws).await.unwrap();
        assert_eq!(a_state.queue.as_ref().map(|q| q.state.as_str()), Some("superseded"));
        assert_eq!(a_state.status, TaskStatus::Queued);
        // Now manually resume the superseded a — must NOT fail; it should
        // start.
        coordinator.start_queued_now("a", &ws).await.unwrap();
        assert_eq!(start_calls(&runner), vec!["b", "a"]);
        let a_after = store.read_task_state("a", &ws).await.unwrap();
        assert_eq!(a_after.queue.as_ref().map(|q| q.state.as_str()), Some("active"));
        assert_eq!(
            a_after.queue.as_ref().and_then(|q| q.activation_source.as_deref()),
            Some("manual")
        );
    }

    #[tokio::test]
    async fn preserves_waiting_order_across_restart() {
        let dir = tempfile::tempdir().unwrap();
        let store = BuddyStore::new(dir.path());
        let ws_a = create_queued(&store, "a", "/tmp/repo", Some("2026-01-01T00:00:01Z")).await;
        create_queued(&store, "b", "/tmp/repo", Some("2026-01-01T00:00:02Z")).await;
        // Simulate restart: brand new coordinator from the same disk root.
        let (_store2, runner, coordinator) = make_coordinator(dir.path());
        coordinator.rebuild_and_reconcile_all().await.unwrap();
        assert_eq!(start_calls(&runner), vec!["a"]);
        assert_eq!(runner.calls.lock()[0].workspace_key, ws_a);
    }

    #[tokio::test]
    async fn restart_recovers_running_queued_task_as_paused_and_blocks_next() {
        let dir = tempfile::tempdir().unwrap();
        let (store, runner, coordinator) = make_coordinator(dir.path());
        let ws = create_queued(&store, "a", "/tmp/repo", Some("2026-01-01T00:00:01Z")).await;
        create_queued(&store, "b", "/tmp/repo", Some("2026-01-01T00:00:02Z")).await;
        // Start a, then simulate crash: leave a in RUNNING_CLAUDE on disk.
        coordinator.reconcile(&ws).await.unwrap();
        assert_eq!(start_calls(&runner), vec!["a"]);
        // New coordinator instance (restart) — recoverInterruptedRuns turns
        // RUNNING→PAUSED.
        let (_store2, runner2, coordinator2) = make_coordinator(dir.path());
        // Mimic service.recoverInterruptedRuns then rebuild.
        for task in store.get_tasks().await {
            if matches!(
                task.status,
                TaskStatus::RunningClaude
                    | TaskStatus::RunningCodex
                    | TaskStatus::RunningCursor
                    | TaskStatus::RunningOpencode
                    | TaskStatus::RunningKimi
                    | TaskStatus::Pinging
            ) {
                settle_status(&store, &task.workspace_key, &task.task_id, TaskStatus::Paused).await;
            }
        }
        coordinator2.rebuild_and_reconcile_all().await.unwrap();
        // b must NOT start because a is PAUSED (blocks).
        assert!(start_calls(&runner2).is_empty());
    }

    #[tokio::test]
    async fn recomputes_queue_after_blocking_task_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let (store, runner, coordinator) = make_coordinator(dir.path());
        let ws = create_queued(&store, "a", "/tmp/repo", Some("2026-01-01T00:00:01Z")).await;
        create_queued(&store, "b", "/tmp/repo", Some("2026-01-01T00:00:02Z")).await;
        // a started and paused.
        coordinator.reconcile(&ws).await.unwrap();
        settle_status(&store, &ws, "a", TaskStatus::Paused).await;
        // Delete a.
        store.delete_task("a", &ws).await.unwrap();
        coordinator.reconcile(&ws).await.unwrap();
        assert_eq!(start_calls(&runner), vec!["a", "b"]);
    }

    #[tokio::test]
    async fn does_not_start_same_task_twice_under_concurrent_reconcile() {
        let dir = tempfile::tempdir().unwrap();
        let (store, runner, coordinator) = make_coordinator(dir.path());
        let ws = create_queued(&store, "q1", "/tmp/repo", None).await;
        // Fire many concurrent reconciles.
        let mut set = tokio::task::JoinSet::new();
        for _ in 0..4 {
            let coordinator = coordinator.clone();
            let ws = ws.clone();
            set.spawn(async move { coordinator.reconcile(&ws).await });
        }
        while let Some(result) = set.join_next().await {
            result.unwrap().unwrap();
        }
        assert_eq!(
            start_calls(&runner)
                .iter()
                .filter(|id| *id == "q1")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn coalesces_many_concurrent_reconcile_requests() {
        let dir = tempfile::tempdir().unwrap();
        let (store, runner, coordinator) = make_coordinator(dir.path());
        let ws = create_queued(&store, "q1", "/tmp/repo", None).await;
        // Fire many concurrent reconciles while a first is in flight.
        let mut set = tokio::task::JoinSet::new();
        for _ in 0..6 {
            let coordinator = coordinator.clone();
            let ws = ws.clone();
            set.spawn(async move { coordinator.reconcile(&ws).await });
        }
        while let Some(result) = set.join_next().await {
            result.unwrap().unwrap();
        }
        // Coalescing guarantees at most one start (the same task is never
        // started twice).
        assert_eq!(
            start_calls(&runner)
                .iter()
                .filter(|id| *id == "q1")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn does_not_write_queue_reconciled_events() {
        let dir = tempfile::tempdir().unwrap();
        let (store, _runner, coordinator) = make_coordinator(dir.path());
        let ws = create_queued(&store, "q1", "/tmp/repo", None).await;
        coordinator.reconcile(&ws).await.unwrap();
        let events = store.get_events("q1", 0, &ws).await;
        assert!(!events.iter().any(|e| e.event_type == "queue.reconciled"));
        // task.queued + queue.activated are still present.
        assert!(events.iter().any(|e| e.event_type == "task.queued"));
        assert!(events.iter().any(|e| e.event_type == "queue.activated"));
    }

    #[tokio::test]
    async fn emits_only_one_queue_blocked_for_identical_signatures() {
        let dir = tempfile::tempdir().unwrap();
        let (store, runner, coordinator) = make_coordinator(dir.path());
        let ws = create_queued(&store, "a", "/tmp/repo", Some("2026-01-01T00:00:01Z")).await;
        create_queued(&store, "b", "/tmp/repo", Some("2026-01-01T00:00:02Z")).await;
        // a started, then paused — blocks b.
        coordinator.reconcile(&ws).await.unwrap();
        settle_status(&store, &ws, "a", TaskStatus::Paused).await;
        coordinator.reconcile(&ws).await.unwrap();
        // Repeated reconciles with the same blocker must not add more
        // queue.blocked events.
        coordinator.reconcile(&ws).await.unwrap();
        coordinator.reconcile(&ws).await.unwrap();
        let events = store.get_events("b", 0, &ws).await;
        assert_eq!(blocked_events(&events).len(), 1);
        assert_eq!(start_calls(&runner), vec!["a"]);
    }

    #[tokio::test]
    async fn emits_new_queue_blocked_when_blocker_changes() {
        let dir = tempfile::tempdir().unwrap();
        let (store, runner, coordinator) = make_coordinator(dir.path());
        let ws = create_queued(&store, "a", "/tmp/repo", Some("2026-01-01T00:00:01Z")).await;
        create_queued(&store, "b", "/tmp/repo", Some("2026-01-01T00:00:02Z")).await;
        // a started and paused — blocks b (reason active_queued_task,
        // blocked_task_id=a).
        coordinator.reconcile(&ws).await.unwrap();
        settle_status(&store, &ws, "a", TaskStatus::Paused).await;
        coordinator.reconcile(&ws).await.unwrap();
        assert_eq!(blocked_events(&store.get_events("b", 0, &ws).await).len(), 1);
        // a recovers to DONE; now an incomplete immediate task becomes the
        // new blocker.
        settle_status(&store, &ws, "a", TaskStatus::Done).await;
        let imm_ws = create_immediate(&store, "imm", "/tmp/repo").await;
        set_status(&store, &imm_ws, "imm", TaskStatus::RunningClaude).await;
        coordinator.reconcile(&ws).await.unwrap();
        let events = store.get_events("b", 0, &ws).await;
        let blocked = blocked_events(&events);
        assert_eq!(blocked.len(), 2);
        assert_eq!(
            blocked[0].payload.get("blocked_task_id").and_then(Value::as_str),
            Some("a")
        );
        assert_eq!(
            blocked[1].payload.get("blocked_task_id").and_then(Value::as_str),
            Some("imm")
        );
        assert_eq!(start_calls(&runner), vec!["a"]);
    }

    #[tokio::test]
    async fn clears_block_signature_after_queue_advances() {
        let dir = tempfile::tempdir().unwrap();
        let (store, runner, coordinator) = make_coordinator(dir.path());
        let ws = create_queued(&store, "a", "/tmp/repo", Some("2026-01-01T00:00:01Z")).await;
        create_queued(&store, "b", "/tmp/repo", Some("2026-01-01T00:00:02Z")).await;
        // a started, paused → blocked(b).
        coordinator.reconcile(&ws).await.unwrap();
        settle_status(&store, &ws, "a", TaskStatus::Paused).await;
        coordinator.reconcile(&ws).await.unwrap();
        assert_eq!(blocked_events(&store.get_events("b", 0, &ws).await).len(), 1);
        // a recovers to DONE → queue advances to b.
        settle_status(&store, &ws, "a", TaskStatus::Done).await;
        coordinator.reconcile(&ws).await.unwrap();
        assert_eq!(start_calls(&runner), vec!["a", "b"]);
        // b DONE, add c, start c, pause c → c blocks, but there is no later
        // waiting task to anchor on, so no new blocked event is recorded and
        // the signature was cleared on advance.
        settle_status(&store, &ws, "b", TaskStatus::Done).await;
        create_queued(&store, "c", "/tmp/repo", Some("2026-01-01T00:00:03Z")).await;
        coordinator.reconcile(&ws).await.unwrap();
        settle_status(&store, &ws, "c", TaskStatus::Paused).await;
        coordinator.reconcile(&ws).await.unwrap();
        let c_events = store.get_events("c", 0, &ws).await;
        assert!(!c_events.iter().any(|e| e.event_type == "queue.blocked"));
    }

    #[tokio::test]
    async fn legacy_failed_task_does_not_block_a_queued_task() {
        let dir = tempfile::tempdir().unwrap();
        let (store, runner, coordinator) = make_coordinator(dir.path());
        let ws = create_queued(&store, "q1", "/tmp/repo", None).await;
        // A legacy task (no execution_mode) stuck in FAILED.
        let legacy_ws = create_immediate(&store, "legacy", "/tmp/repo").await;
        write_legacy_state(&store, "legacy", &legacy_ws, "FAILED");
        coordinator.reconcile(&ws).await.unwrap();
        assert_eq!(start_calls(&runner), vec!["q1"]);
    }

    #[tokio::test]
    async fn legacy_paused_and_ready_tasks_do_not_block_a_queued_task() {
        let dir = tempfile::tempdir().unwrap();
        let (store, runner, coordinator) = make_coordinator(dir.path());
        let ws = create_queued(&store, "q1", "/tmp/repo", None).await;
        for status in ["PAUSED", "READY"] {
            let id = format!("legacy-{status}");
            let legacy_ws = create_immediate(&store, &id, "/tmp/repo").await;
            write_legacy_state(&store, &id, &legacy_ws, status);
        }
        coordinator.reconcile(&ws).await.unwrap();
        assert_eq!(start_calls(&runner), vec!["q1"]);
    }

    #[tokio::test]
    async fn legacy_running_pinging_countdown_tasks_still_block_the_queue() {
        let dir = tempfile::tempdir().unwrap();
        let (store, runner, coordinator) = make_coordinator(dir.path());
        let ws = create_queued(&store, "q1", "/tmp/repo", None).await;
        for status in ["RUNNING_CLAUDE", "PINGING", "COUNTDOWN"] {
            let id = format!("legacy-{status}");
            let legacy_ws = create_immediate(&store, &id, "/tmp/repo").await;
            write_legacy_state(&store, &id, &legacy_ws, status);
        }
        coordinator.reconcile(&ws).await.unwrap();
        // None of the running legacy tasks let q1 start.
        assert!(!start_calls(&runner).contains(&"q1".to_string()));
    }

    #[tokio::test]
    async fn explicit_immediate_failed_task_still_blocks_the_queue() {
        let dir = tempfile::tempdir().unwrap();
        let (store, runner, coordinator) = make_coordinator(dir.path());
        let ws = create_queued(&store, "q1", "/tmp/repo", None).await;
        let imm_ws = create_immediate(&store, "imm", "/tmp/repo").await;
        // Explicit immediate (execution_mode set by create_task) then FAILED.
        set_status(&store, &imm_ws, "imm", TaskStatus::Failed).await;
        coordinator.reconcile(&ws).await.unwrap();
        assert!(!start_calls(&runner).contains(&"q1".to_string()));
    }

    #[tokio::test]
    async fn explicit_immediate_paused_task_still_blocks_the_queue() {
        let dir = tempfile::tempdir().unwrap();
        let (store, runner, coordinator) = make_coordinator(dir.path());
        let ws = create_queued(&store, "q1", "/tmp/repo", None).await;
        let imm_ws = create_immediate(&store, "imm", "/tmp/repo").await;
        set_status(&store, &imm_ws, "imm", TaskStatus::Paused).await;
        coordinator.reconcile(&ws).await.unwrap();
        assert!(!start_calls(&runner).contains(&"q1".to_string()));
    }

    #[tokio::test]
    async fn reads_legacy_state_without_queue_fields_as_immediate() {
        let dir = tempfile::tempdir().unwrap();
        let store = BuddyStore::new(dir.path());
        let created = store
            .create_task(CreateTaskInput {
                task_id: "legacy".to_string(),
                repo_root: Some("/tmp/repo".to_string()),
                task_text: None,
                context_text: None,
                settings: None,
                execution_mode: None,
            })
            .await
            .unwrap();
        // Overwrite state.json with a minimal legacy shape (no
        // execution_mode / queue).
        let value = serde_json::json!({
            "protocol_version": "1",
            "task_id": "legacy",
            "repo_root": "/tmp/repo",
            "status": "READY",
            "round": 0,
            "next_actor": "claude",
            "active_run": null,
            "instruction_queue": []
        });
        std::fs::write(
            store.task_directory("legacy", &created.workspace_key).join("state.json"),
            serde_json::to_string(&value).unwrap(),
        )
        .unwrap();
        let state = store
            .read_task_state("legacy", &created.workspace_key)
            .await
            .unwrap();
        assert_eq!(state.status, TaskStatus::Ready);
        assert_eq!(state.execution_mode.unwrap_or(ExecutionMode::Immediate), ExecutionMode::Immediate);
        assert!(state.execution_mode.is_none());
        assert!(state.queue.is_none());
        // getTasks should surface it as immediate.
        let tasks = store.get_tasks().await;
        let legacy = tasks.iter().find(|t| t.task_id == "legacy").unwrap();
        assert_eq!(
            legacy.execution_mode.unwrap_or(ExecutionMode::Immediate),
            ExecutionMode::Immediate
        );
    }
}
