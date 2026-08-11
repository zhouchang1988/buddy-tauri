//! System notifications — Rust port of `src/main/buddy/notifications.ts`.
//!
//! Sends macOS system notifications on task DONE / FAILED / PAUSED via
//! tauri-plugin-notification, gated by
//! `GlobalSettings.system_notifications_enabled` (default: on). The
//! production implementation of the runner's `TaskNotifier` trait lives here;
//! a recording sender keeps the whole flow unit-testable without firing real
//! notifications.

use std::sync::Arc;

use async_trait::async_trait;
use tauri::AppHandle;
use tauri_plugin_notification::NotificationExt;

use super::prompts::actor_display_name;
use super::runner::TaskNotifier;
use super::store::BuddyStore;
use super::types::TaskStats;

/// Maximum length for error reason in notification body.
const MAX_ERROR_LENGTH: usize = 120;

/// Reason strings used by the runner (see `runner.rs`).
pub const REASON_DUAL_BREAK_CONFIRMED: &str = "dual_break_confirmed";
pub const REASON_BREAK_CONFIRMED_ON_FAILURE: &str = "break_confirmed_on_failure";

/// Delivery sink for a rendered notification. Splitting this out keeps the
/// gating/body logic testable without a running app.
pub trait NotificationSender: Send + Sync {
    /// Errors are swallowed by callers — notifications are never critical.
    fn send(&self, title: &str, body: &str);
}

/// Production sender backed by tauri-plugin-notification.
pub struct SystemNotificationSender {
    app: AppHandle,
}

impl SystemNotificationSender {
    pub fn new(app: AppHandle) -> Self {
        Self { app }
    }
}

impl NotificationSender for SystemNotificationSender {
    fn send(&self, title: &str, body: &str) {
        let _ = self
            .app
            .notification()
            .builder()
            .title(title)
            .body(body)
            .show();
    }
}

/// Create the production `TaskNotifier` (port of `createTaskNotifier`).
pub fn create_task_notifier(app: AppHandle, store: Arc<BuddyStore>) -> Arc<dyn TaskNotifier> {
    Arc::new(SystemTaskNotifier::new(
        store,
        Arc::new(SystemNotificationSender::new(app)),
    ))
}

/// A notifier that drops everything — for tests and headless runs.
pub fn create_noop_notifier() -> Arc<dyn TaskNotifier> {
    Arc::new(NoopTaskNotifier)
}

pub struct NoopTaskNotifier;

#[async_trait]
impl TaskNotifier for NoopTaskNotifier {
    async fn notify_task_done(
        &self,
        _task_id: &str,
        _workspace_key: &str,
        _reason: &str,
        _first_actor: Option<&str>,
        _second_actor: Option<&str>,
    ) {
    }
    async fn notify_task_failed(
        &self,
        _task_id: &str,
        _workspace_key: &str,
        _actor: &str,
        _error: &str,
    ) {
    }
    async fn notify_task_paused(
        &self,
        _task_id: &str,
        _workspace_key: &str,
        _actor: &str,
        _consecutive_failures: u32,
        _max_failures: u32,
    ) {
    }
}

/// `TaskNotifier` implementation reading the enabled-flag from the store and
/// delegating delivery to a [`NotificationSender`].
pub struct SystemTaskNotifier {
    store: Arc<BuddyStore>,
    sender: Arc<dyn NotificationSender>,
}

impl SystemTaskNotifier {
    pub fn new(store: Arc<BuddyStore>, sender: Arc<dyn NotificationSender>) -> Self {
        Self { store, sender }
    }

    async fn is_enabled(&self) -> bool {
        match self.store.read_global_settings().await {
            Ok(settings) => settings.system_notifications_enabled.unwrap_or(true),
            Err(_) => true,
        }
    }
}

#[async_trait]
impl TaskNotifier for SystemTaskNotifier {
    async fn notify_task_done(
        &self,
        task_id: &str,
        workspace_key: &str,
        reason: &str,
        first_actor: Option<&str>,
        second_actor: Option<&str>,
    ) {
        if !self.is_enabled().await {
            return;
        }
        let body = if reason == REASON_BREAK_CONFIRMED_ON_FAILURE {
            if let (Some(first), Some(second)) = (first_actor, second_actor) {
                format!(
                    "任务：{task_id}\n状态：已完成\n{} 请求结束，{} 执行失败后自动确认结束。",
                    actor_display_name(first),
                    actor_display_name(second)
                )
            } else {
                done_body_with_stats(task_id, self.store.get_task_stats(task_id, workspace_key).await)
            }
        } else {
            done_body_with_stats(task_id, self.store.get_task_stats(task_id, workspace_key).await)
        };
        self.sender.send("Buddy - 任务已完成", &body);
    }

    async fn notify_task_failed(
        &self,
        task_id: &str,
        _workspace_key: &str,
        actor: &str,
        error: &str,
    ) {
        if !self.is_enabled().await {
            return;
        }
        let truncated_error = truncate_chars(error, MAX_ERROR_LENGTH);
        let body = format!(
            "任务：{task_id}\n状态：失败\nActor：{}\n原因：{truncated_error}",
            actor_display_name(actor)
        );
        self.sender.send("Buddy - 任务失败", &body);
    }

    async fn notify_task_paused(
        &self,
        task_id: &str,
        _workspace_key: &str,
        actor: &str,
        consecutive_failures: u32,
        max_failures: u32,
    ) {
        if !self.is_enabled().await {
            return;
        }
        let body = format!(
            "任务：{task_id}\n状态：已暂停\n{} 连续失败 {consecutive_failures} 次，已达到上限 ({max_failures})，等待用户处理。",
            actor_display_name(actor)
        );
        self.sender.send("Buddy - 任务已暂停", &body);
    }
}

fn done_body_with_stats(task_id: &str, stats: Option<TaskStats>) -> String {
    match stats {
        Some(stats) => format!(
            "任务：{task_id}\n状态：已完成\n合计：{} 轮 · {} · 输入 {} · 输出 {} · Cache {}",
            stats.total_rounds,
            format_duration(stats.total_duration_ms),
            format_number(stats.total_input_tokens),
            format_number(stats.total_output_tokens),
            format_number(stats.total_cache_read_tokens),
        ),
        None => format!("任务：{task_id}\n状态：已完成\n双方均已确认任务结束。"),
    }
}

/// JS `string.slice` + `+ '...'` on chars.
fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    format!("{}...", text.chars().take(max).collect::<String>())
}

/// Format milliseconds into a human-readable duration string (xdxhxmxs).
pub fn format_duration(ms: u64) -> String {
    if ms < 1000 {
        return format!("{ms}ms");
    }
    let total_seconds = ms / 1000;
    let days = total_seconds / 86400;
    let hours = (total_seconds % 86400) / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let seconds = total_seconds % 60;
    let mut parts = Vec::new();
    if days > 0 {
        parts.push(format!("{days}d"));
    }
    if days > 0 || hours > 0 {
        parts.push(format!("{hours}h"));
    }
    if days > 0 || hours > 0 || minutes > 0 {
        parts.push(format!("{minutes}m"));
    }
    parts.push(format!("{seconds}s"));
    parts.join("")
}

/// Format a number with comma thousands separators (JS `toLocaleString()` in
/// the default Node ICU locale).
pub fn format_number(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;

    #[derive(Default)]
    struct RecordingSender {
        sent: Mutex<Vec<(String, String)>>,
    }

    impl NotificationSender for RecordingSender {
        fn send(&self, title: &str, body: &str) {
            self.sent.lock().push((title.to_string(), body.to_string()));
        }
    }

    fn test_notifier(
        store: Arc<BuddyStore>,
    ) -> (SystemTaskNotifier, Arc<RecordingSender>) {
        let sender = Arc::new(RecordingSender::default());
        (
            SystemTaskNotifier::new(store, sender.clone()),
            sender,
        )
    }

    async fn store_with_notifications_enabled(enabled: Option<bool>) -> Arc<BuddyStore> {
        let dir = tempfile::tempdir().unwrap();
        // Keep the tempdir alive by leaking it into the store's data root —
        // tests are short-lived processes.
        let data_root = dir.path().join("data");
        std::mem::forget(dir);
        let store = Arc::new(BuddyStore::new(data_root));
        if let Some(enabled) = enabled {
            store
                .update_global_settings(&crate::buddy::types::GlobalSettings {
                    system_notifications_enabled: Some(enabled),
                    ..Default::default()
                })
                .await
                .unwrap();
        }
        store
    }

    #[test]
    fn duration_formatting() {
        assert_eq!(format_duration(0), "0ms");
        assert_eq!(format_duration(999), "999ms");
        assert_eq!(format_duration(1000), "1s");
        assert_eq!(format_duration(61_000), "1m1s");
        assert_eq!(format_duration(3_600_000), "1h0m0s");
        assert_eq!(format_duration(90_061_000), "1d1h1m1s");
    }

    #[test]
    fn number_formatting() {
        assert_eq!(format_number(0), "0");
        assert_eq!(format_number(999), "999");
        assert_eq!(format_number(1_000), "1,000");
        assert_eq!(format_number(1_234_567), "1,234,567");
    }

    #[test]
    fn error_truncation() {
        let long = "x".repeat(200);
        let truncated = truncate_chars(&long, MAX_ERROR_LENGTH);
        assert_eq!(truncated.chars().count(), MAX_ERROR_LENGTH + 3);
        assert!(truncated.ends_with("..."));
        assert_eq!(truncate_chars("short", MAX_ERROR_LENGTH), "short");
    }

    #[tokio::test]
    async fn failed_notification_body_and_gating() {
        let store = store_with_notifications_enabled(None).await;
        let (notifier, sender) = test_notifier(store);
        notifier
            .notify_task_failed("task-1", "ws", "claude", "boom")
            .await;
        let sent = sender.sent.lock();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].0, "Buddy - 任务失败");
        assert_eq!(
            sent[0].1,
            "任务：task-1\n状态：失败\nActor：Claude Code\n原因：boom"
        );
    }

    #[tokio::test]
    async fn disabled_setting_suppresses_all_notifications() {
        let store = store_with_notifications_enabled(Some(false)).await;
        let (notifier, sender) = test_notifier(store);
        notifier
            .notify_task_failed("t", "ws", "claude", "err")
            .await;
        notifier
            .notify_task_paused("t", "ws", "codex", 3, 3)
            .await;
        notifier
            .notify_task_done("t", "ws", REASON_DUAL_BREAK_CONFIRMED, None, None)
            .await;
        assert!(sender.sent.lock().is_empty());
    }

    #[tokio::test]
    async fn paused_notification_body() {
        let store = store_with_notifications_enabled(Some(true)).await;
        let (notifier, sender) = test_notifier(store);
        notifier
            .notify_task_paused("task-9", "ws", "opencode", 3, 3)
            .await;
        let sent = sender.sent.lock();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].0, "Buddy - 任务已暂停");
        assert_eq!(
            sent[0].1,
            "任务：task-9\n状态：已暂停\nOpenCode 连续失败 3 次，已达到上限 (3)，等待用户处理。"
        );
    }

    #[tokio::test]
    async fn done_break_confirmed_on_failure_body() {
        let store = store_with_notifications_enabled(None).await;
        let (notifier, sender) = test_notifier(store);
        notifier
            .notify_task_done(
                "task-2",
                "ws",
                REASON_BREAK_CONFIRMED_ON_FAILURE,
                Some("claude"),
                Some("codex"),
            )
            .await;
        let sent = sender.sent.lock();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].0, "Buddy - 任务已完成");
        assert_eq!(
            sent[0].1,
            "任务：task-2\n状态：已完成\nClaude Code 请求结束，Codex 执行失败后自动确认结束。"
        );
    }

    #[tokio::test]
    async fn done_without_stats_falls_back_to_default_body() {
        let store = store_with_notifications_enabled(None).await;
        let (notifier, sender) = test_notifier(store);
        // No events recorded for this task → stats are None.
        notifier
            .notify_task_done("ghost", "ws", REASON_DUAL_BREAK_CONFIRMED, None, None)
            .await;
        let sent = sender.sent.lock();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].1, "任务：ghost\n状态：已完成\n双方均已确认任务结束。");
    }
}
