//! Auto-updater — Rust port of `src/main/updater.ts`.
//!
//! electron-updater flow (check → available/not-available → download →
//! progress → downloaded → install) on top of tauri-plugin-updater, forwarding
//! `updater:event` payloads to the renderer with the exact shapes
//! `src/hooks/useUpdater.ts` expects.
//!
//! `lib.rs` wiring: call [`init_updater`] during setup, and route the
//! `updater_check` / `updater_download` / `updater_install` commands to
//! [`check_for_updates`] / [`download_update`] / [`install_update`].

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_updater::{Update, UpdaterExt};

use crate::buddy::redact::redact_sensitive_text;

/// Which updater operation is in flight — serialized as
/// `'check' | 'download' | 'install'` in `error` events, matching the
/// renderer's `UpdaterPhase` in `src/hooks/useUpdater.ts`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum UpdaterPhase {
    Check,
    Download,
    Install,
}

/// `updater:event` payload, serialized with a `type` tag — matches the
/// renderer's `UpdaterEvent` union in `src/hooks/useUpdater.ts`.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum UpdaterEvent {
    Checking,
    Available { info: UpdateInfo },
    #[serde(rename = "not-available")]
    NotAvailable,
    Progress { progress: DownloadProgress },
    Downloaded { info: UpdateInfo },
    Installing { version: String },
    Error { phase: UpdaterPhase, message: String },
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateInfo {
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub release_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mandatory: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadProgress {
    pub bytes_per_second: f64,
    pub percent: f64,
    pub transferred: u64,
    pub total: u64,
}

/// Who initiated the current check — controls whether the `checking` state and
/// a check-phase failure are user-visible (port of TS `UpdateCheckOrigin`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckOrigin {
    Background,
    Manual,
}

/// Managed updater state (TS module-level `mainWindow`/`initialized`).
pub struct UpdaterState {
    /// Update found by the latest check but not yet downloaded.
    latest: Mutex<Option<Update>>,
    /// Downloaded update ready to install (update + verified bytes).
    downloaded: Mutex<Option<(Update, Vec<u8>)>>,
    /// Version of the downloaded update, for `downloaded` re-emission.
    downloaded_info: Mutex<Option<UpdateInfo>>,
    downloading: AtomicBool,
    /// Tracks the current updater phase so failures can report which
    /// operation failed (TS module-level `currentPhase`).
    current_phase: Mutex<UpdaterPhase>,
    /// Set to `false` when the user dismisses an update error — the periodic
    /// re-check loop then stops so a failed update is not retried (and the
    /// error notification does not pop up again). Manual checks still work.
    auto_retry: AtomicBool,
    /// Origin of the in-flight check (TS `currentCheckOrigin`). `None` means
    /// no check is in flight; a background check is silent about the
    /// `checking` state and check-phase errors unless promoted to manual.
    current_check_origin: Mutex<Option<CheckOrigin>>,
    /// Single-flight guard so overlapping checks don't stack
    /// (TS `checkInProgress`).
    check_in_progress: AtomicBool,
    /// Suppresses duplicate error dispatch for one failure (same phase +
    /// redacted message within a single operation). Reset at the start of
    /// every check/download so a repeated identical failure across separate
    /// operations is still surfaced (TS `lastDispatchedError`).
    last_dispatched_error: Mutex<Option<(UpdaterPhase, String)>>,
}

impl UpdaterState {
    fn new() -> Self {
        Self {
            latest: Mutex::new(None),
            downloaded: Mutex::new(None),
            downloaded_info: Mutex::new(None),
            downloading: AtomicBool::new(false),
            current_phase: Mutex::new(UpdaterPhase::Check),
            auto_retry: AtomicBool::new(true),
            current_check_origin: Mutex::new(None),
            check_in_progress: AtomicBool::new(false),
            last_dispatched_error: Mutex::new(None),
        }
    }

    /// Single-flight entry point. Returns `true` when this call owns the
    /// check; `false` when one is already in flight — a manual request then
    /// promotes an in-flight background check so its failure becomes visible.
    fn begin_check(&self, origin: CheckOrigin) -> bool {
        if self.check_in_progress.swap(true, Ordering::SeqCst) {
            if origin == CheckOrigin::Manual {
                let mut current = self.current_check_origin.lock();
                if *current == Some(CheckOrigin::Background) {
                    *current = Some(CheckOrigin::Manual);
                }
            }
            return false;
        }
        *self.current_check_origin.lock() = Some(origin);
        true
    }

    fn end_check(&self) {
        *self.current_check_origin.lock() = None;
        self.check_in_progress.store(false, Ordering::SeqCst);
    }

    fn check_origin(&self) -> Option<CheckOrigin> {
        *self.current_check_origin.lock()
    }

    /// Records (phase, message); returns `false` when it exactly repeats the
    /// previous dispatch within this operation (drop the duplicate).
    fn should_dispatch_error(&self, phase: UpdaterPhase, message: &str) -> bool {
        let mut last = self.last_dispatched_error.lock();
        if last.as_ref() == Some(&(phase, message.to_string())) {
            return false;
        }
        *last = Some((phase, message.to_string()));
        true
    }

    fn reset_error_dedup(&self) {
        *self.last_dispatched_error.lock() = None;
    }
}

fn state(app: &AppHandle) -> Arc<UpdaterState> {
    app.state::<Arc<UpdaterState>>().inner().clone()
}

fn set_phase(app: &AppHandle, phase: UpdaterPhase) {
    *state(app).current_phase.lock() = phase;
}

/// TS `sendError`: redact secrets from the raw message before it reaches the
/// renderer, with a fallback for empty messages. Dedups an exact repeat
/// (same phase + redacted message) within one operation.
fn send_error(app: &AppHandle, phase: UpdaterPhase, error: &str) -> String {
    let redacted = redact_sensitive_text(error);
    let message = if redacted.is_empty() {
        "Unknown update error".to_string()
    } else {
        redacted
    };
    if !state(app).should_dispatch_error(phase, &message) {
        return message;
    }
    send_to_renderer(app, UpdaterEvent::Error {
        phase,
        message: message.clone(),
    });
    message
}

fn send_to_renderer(app: &AppHandle, event: UpdaterEvent) {
    // TS sends to the main window only; `app.emit` targets every webview,
    // which is equivalent here (single window).
    let _ = app.emit("updater:event", event);
}

fn update_info(update: &Update) -> UpdateInfo {
    UpdateInfo {
        version: update.version.clone(),
        // electron-updater surfaces the release feed's raw date string; the
        // tauri feed's `pub_date` is the same value.
        release_date: update
            .raw_json
            .get("pub_date")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        mandatory: match update.raw_json.get("mandatory") {
            Some(serde_json::Value::Bool(true)) => Some(true),
            Some(_) => Some(false),
            None => None,
        },
    }
}

/// Whether auto-update is enabled. tauri-plugin-updater 2.x ignores the
/// `active` key in the plugin config (its `Config` struct has no such
/// field), so we read it ourselves as our own on/off switch. Defaults to
/// `true` when the key is absent, keeping older configs working.
fn updater_active_from(plugin_config: Option<&serde_json::Value>) -> bool {
    plugin_config
        .and_then(|config| config.get("active"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(true)
}

pub(crate) fn updater_active(app: &AppHandle) -> bool {
    updater_active_from(app.config().plugins.0.get("updater"))
}

/// TS `runCheck`: single-flight check runner with origin promotion.
/// - background checks that overlap an in-flight check are skipped;
/// - manual checks that overlap a background check promote it to manual, so a
///   failure becomes user-visible.
async fn run_check(app: AppHandle, origin: CheckOrigin) -> Result<(), String> {
    let st = state(&app);
    if !st.begin_check(origin) {
        return Ok(());
    }
    st.reset_error_dedup();
    drop(st);
    let result = run_check_inner(&app).await;
    state(&app).end_check();
    result
}

/// Run one check cycle: emits `checking` (manual checks only), then
/// `available` (and kicks off the auto-download, like `autoDownload = true`)
/// or `not-available`. Check failures emit an `error` event with phase
/// `check` only for manual checks (background checks stay silent) and are
/// returned as `Err` either way.
async fn run_check_inner(app: &AppHandle) -> Result<(), String> {
    let is_manual = state(app).check_origin() == Some(CheckOrigin::Manual);
    if !updater_active(app) {
        // Updater disabled in tauri.conf.json (`active: false`) — no update
        // server is configured yet, so every check would 404. Manual checks
        // get a clear message instead of a network error.
        let message = "Auto-update is disabled in this build (no update server configured yet).";
        let message = if is_manual {
            send_error(app, UpdaterPhase::Check, message)
        } else {
            message.to_string()
        };
        return Err(message);
    }
    set_phase(app, UpdaterPhase::Check);
    if is_manual {
        send_to_renderer(app, UpdaterEvent::Checking);
    }
    let updater = match app.updater() {
        Ok(updater) => updater,
        Err(error) => {
            let message = error.to_string();
            let message = if is_manual {
                send_error(app, UpdaterPhase::Check, &message)
            } else {
                redact_sensitive_text(&message)
            };
            return Err(message);
        }
    };
    match updater.check().await {
        Ok(Some(update)) => {
            let info = update_info(&update);
            let st = state(app);
            {
                let mut downloaded = st.downloaded.lock();
                *downloaded = None;
                *st.downloaded_info.lock() = None;
            }
            *st.latest.lock() = Some(update);
            drop(st);
            // autoDownload=true: the download starts immediately.
            set_phase(app, UpdaterPhase::Download);
            send_to_renderer(app, UpdaterEvent::Available { info });
            // autoDownload parity: start downloading right away. A failure
            // emits its own `error` event inside `download_update`.
            let _ = download_update(app).await;
            Ok(())
        }
        Ok(None) => {
            send_to_renderer(app, UpdaterEvent::NotAvailable);
            Ok(())
        }
        Err(error) => {
            let message = error.to_string();
            let message = if is_manual {
                send_error(app, UpdaterPhase::Check, &message)
            } else {
                redact_sensitive_text(&message)
            };
            Err(message)
        }
    }
}

/// TS: `initUpdater` — registers state, then checks after a 5s startup delay
/// and every 30 minutes thereafter. When the updater is disabled in
/// `tauri.conf.json` (`active: false`), only the state is registered (so the
/// commands keep working) and no automatic checks are scheduled.
pub fn init_updater(app: &AppHandle) {
    if app.try_state::<Arc<UpdaterState>>().is_some() {
        return; // initialized
    }
    app.manage(Arc::new(UpdaterState::new()));

    if !updater_active(app) {
        return; // updater disabled — no periodic checks
    }

    let app_handle = app.clone();
    tauri::async_runtime::spawn(async move {
        // Delay first check to avoid impacting startup.
        tokio::time::sleep(Duration::from_secs(5)).await;
        loop {
            if !state(&app_handle).auto_retry.load(Ordering::SeqCst) {
                // User dismissed an update error — stop the periodic loop so
                // the failure is not retried automatically. Checked before
                // each check so a dismissal during the 30-minute sleep also
                // prevents the next run.
                break;
            }
            let _ = run_check(app_handle.clone(), CheckOrigin::Background).await;
            tokio::time::sleep(Duration::from_secs(30 * 60)).await;
        }
    });
}

/// TS: dismissing the update-error notification. Stops the periodic
/// re-check loop so a failed update is not retried until the next manual
/// check or app restart.
pub fn stop_auto_retry(app: &AppHandle) {
    state(app).auto_retry.store(false, Ordering::SeqCst);
}

/// TS: `checkForUpdates` — a manual check. Emits `checking`/
/// `available`/`not-available` as before; on failure emits `error` with phase
/// `check` and returns the (redacted) message so the command can reject.
/// Overlapping an in-flight background check promotes it instead of stacking.
pub async fn check_for_updates(app: &AppHandle) -> Result<(), String> {
    run_check(app.clone(), CheckOrigin::Manual).await
}

/// TS: `downloadUpdate`. With auto-download already running this is usually
/// a no-op; if a check found an update that has not started downloading yet,
/// this kicks it off. Safe to call repeatedly. On failure emits `error` with
/// phase `download` and returns the (redacted) message.
pub async fn download_update(app: &AppHandle) -> Result<(), String> {
    set_phase(app, UpdaterPhase::Download);
    // Reset per-operation dedup so a repeated identical download error across
    // separate downloads is still surfaced.
    state(app).reset_error_dedup();
    let st = state(app);
    if st.downloaded.lock().is_some() {
        // Already downloaded — re-announce so late listeners catch up.
        if let Some(info) = st.downloaded_info.lock().clone() {
            set_phase(app, UpdaterPhase::Install);
            send_to_renderer(app, UpdaterEvent::Downloaded { info });
        }
        return Ok(());
    }
    if st.downloading.swap(true, Ordering::SeqCst) {
        return Ok(());
    }
    let update = st.latest.lock().clone();
    let Some(update) = update else {
        st.downloading.store(false, Ordering::SeqCst);
        return Ok(());
    };

    let app_handle = app.clone();
    let transferred = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let started = Instant::now();
    let progress_transferred = transferred.clone();
    let progress_app = app_handle.clone();
    // tauri-plugin-updater fires the progress callback per network chunk,
    // which means thousands of `updater:event` emits per download — each one
    // a separate JS task on the webview main thread that competes with
    // keyboard input. Throttle to one emit per 250ms (always emit the first
    // chunk and the completing chunk).
    let last_progress_emit_ms = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let result = update
        .download(
            move |chunk_len, content_len| {
                let transferred =
                    progress_transferred.fetch_add(chunk_len as u64, Ordering::SeqCst)
                        + chunk_len as u64;
                let total = content_len.unwrap_or(0);
                let elapsed = started.elapsed().as_secs_f64();
                let bytes_per_second = if elapsed > 0.0 {
                    transferred as f64 / elapsed
                } else {
                    0.0
                };
                let percent = if total > 0 {
                    transferred as f64 / total as f64 * 100.0
                } else {
                    0.0
                };
                let now_ms = (started.elapsed().as_millis() as u64).max(1);
                let last_ms = last_progress_emit_ms.load(Ordering::SeqCst);
                let is_first = last_ms == 0;
                let is_final = total > 0 && transferred >= total;
                if !is_first && !is_final && now_ms.saturating_sub(last_ms) < 250 {
                    return;
                }
                last_progress_emit_ms.store(now_ms, Ordering::SeqCst);
                send_to_renderer(
                    &progress_app,
                    UpdaterEvent::Progress {
                        progress: DownloadProgress {
                            bytes_per_second,
                            percent,
                            transferred,
                            total,
                        },
                    },
                );
            },
            || {},
        )
        .await;

    let st = state(&app_handle);
    st.downloading.store(false, Ordering::SeqCst);
    match result {
        Ok(bytes) => {
            let info = update_info(&update);
            *st.downloaded.lock() = Some((update, bytes));
            *st.downloaded_info.lock() = Some(info.clone());
            set_phase(&app_handle, UpdaterPhase::Install);
            send_to_renderer(&app_handle, UpdaterEvent::Downloaded { info });
            Ok(())
        }
        Err(error) => {
            // v1.2.14: download failures surface as an `error` event with
            // phase `download` (they used to map to `not-available`).
            let message = send_error(&app_handle, UpdaterPhase::Download, &error.to_string());
            Err(message)
        }
    }
}

/// TS: `quitAndInstall`. No-op when nothing has been downloaded. Emits
/// `installing` before installing the verified bytes and restarts the app
/// (the restart path does not return). On install failure it emits `error`
/// with phase `install` and keeps the downloaded update so a retry works.
pub async fn install_update(app: &AppHandle) -> Result<(), String> {
    set_phase(app, UpdaterPhase::Install);
    let downloaded = state(app).downloaded.lock().clone();
    if let Some((update, bytes)) = downloaded {
        send_to_renderer(app, UpdaterEvent::Installing {
            version: update.version.clone(),
        });
        match update.install(&bytes) {
            Ok(()) => app.restart(),
            Err(error) => {
                send_error(app, UpdaterPhase::Install, &error.to_string());
                return Err(error.to_string());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_payload_shapes_match_frontend() {
        assert_eq!(
            serde_json::to_value(UpdaterEvent::Checking).unwrap(),
            serde_json::json!({ "type": "checking" })
        );
        assert_eq!(
            serde_json::to_value(UpdaterEvent::NotAvailable).unwrap(),
            serde_json::json!({ "type": "not-available" })
        );
        assert_eq!(
            serde_json::to_value(UpdaterEvent::Available {
                info: UpdateInfo {
                    version: "1.3.0".into(),
                    release_date: Some("2026-08-01T00:00:00Z".into()),
                    mandatory: Some(true),
                },
            })
            .unwrap(),
            serde_json::json!({
                "type": "available",
                "info": {
                    "version": "1.3.0",
                    "releaseDate": "2026-08-01T00:00:00Z",
                    "mandatory": true,
                },
            })
        );
        assert_eq!(
            serde_json::to_value(UpdaterEvent::Progress {
                progress: DownloadProgress {
                    bytes_per_second: 1024.5,
                    percent: 42.0,
                    transferred: 4200,
                    total: 10_000,
                },
            })
            .unwrap(),
            serde_json::json!({
                "type": "progress",
                "progress": {
                    "bytesPerSecond": 1024.5,
                    "percent": 42.0,
                    "transferred": 4200,
                    "total": 10_000,
                },
            })
        );
        // `releaseDate` is optional in the downloaded payload.
        assert_eq!(
            serde_json::to_value(UpdaterEvent::Downloaded {
                info: UpdateInfo {
                    version: "1.3.0".into(),
                    release_date: None,
                    mandatory: None,
                },
            })
            .unwrap(),
            serde_json::json!({
                "type": "downloaded",
                "info": { "version": "1.3.0" },
            })
        );
        // v1.2.14: `installing` and `error` variants.
        assert_eq!(
            serde_json::to_value(UpdaterEvent::Installing {
                version: "1.3.0".into(),
            })
            .unwrap(),
            serde_json::json!({ "type": "installing", "version": "1.3.0" })
        );
        assert_eq!(
            serde_json::to_value(UpdaterEvent::Error {
                phase: UpdaterPhase::Check,
                message: "network down".into(),
            })
            .unwrap(),
            serde_json::json!({
                "type": "error",
                "phase": "check",
                "message": "network down",
            })
        );
    }

    #[test]
    fn updater_active_defaults_true_and_reads_flag() {
        // No updater plugin config at all -> enabled (backwards compatible).
        assert!(updater_active_from(None));
        // Config present but no `active` key -> enabled.
        assert!(updater_active_from(Some(&serde_json::json!({
            "endpoints": ["https://example.com/latest.json"]
        }))));
        assert!(updater_active_from(Some(&serde_json::json!({ "active": true }))));
        assert!(!updater_active_from(Some(&serde_json::json!({ "active": false }))));
    }

    #[test]
    fn updater_phase_serializes_as_renderer_union() {
        assert_eq!(
            serde_json::to_value(UpdaterPhase::Check).unwrap(),
            serde_json::json!("check")
        );
        assert_eq!(
            serde_json::to_value(UpdaterPhase::Download).unwrap(),
            serde_json::json!("download")
        );
        assert_eq!(
            serde_json::to_value(UpdaterPhase::Install).unwrap(),
            serde_json::json!("install")
        );
        for phase in [UpdaterPhase::Download, UpdaterPhase::Install] {
            assert_eq!(
                serde_json::to_value(UpdaterEvent::Error {
                    phase,
                    message: "x".into(),
                })
                .unwrap()["phase"],
                serde_json::to_value(phase).unwrap()
            );
        }
    }

    // -------------------------------------------------------------------
    // v1.2.15: manual/background origin, single-flight, per-operation dedup
    // -------------------------------------------------------------------

    #[test]
    fn begin_check_is_single_flight_and_manual_promotes_background() {
        let st = UpdaterState::new();
        assert!(st.begin_check(CheckOrigin::Background));
        assert_eq!(st.check_origin(), Some(CheckOrigin::Background));
        // Overlapping background check is skipped.
        assert!(!st.begin_check(CheckOrigin::Background));
        assert_eq!(st.check_origin(), Some(CheckOrigin::Background));
        // A manual check overlapping a background check promotes the origin.
        assert!(!st.begin_check(CheckOrigin::Manual));
        assert_eq!(st.check_origin(), Some(CheckOrigin::Manual));
        st.end_check();
        assert_eq!(st.check_origin(), None);
        // The next check can start fresh.
        assert!(st.begin_check(CheckOrigin::Manual));
        assert_eq!(st.check_origin(), Some(CheckOrigin::Manual));
        st.end_check();
    }

    #[test]
    fn error_dedup_suppresses_exact_repeat_within_one_operation() {
        let st = UpdaterState::new();
        assert!(st.should_dispatch_error(UpdaterPhase::Check, "network down"));
        // Same phase + message → duplicate, dropped.
        assert!(!st.should_dispatch_error(UpdaterPhase::Check, "network down"));
        // Different phase or message still dispatches.
        assert!(st.should_dispatch_error(UpdaterPhase::Download, "network down"));
        assert!(st.should_dispatch_error(UpdaterPhase::Download, "other failure"));
        // Dedup is per-operation: after a reset the identical error surfaces
        // again (e.g. retry after a network failure).
        st.reset_error_dedup();
        assert!(st.should_dispatch_error(UpdaterPhase::Download, "other failure"));
    }
}
