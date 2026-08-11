//! Buddy (Tauri edition) — application entry point.
//!
//! Final wiring, port of `src/main/index.ts` from the Electron edition:
//! fix_shell_path → construct BuddyEventBus/BuddyStore/BuddyRunner/
//! QueueCoordinator/BuddyCoreService → recover_interrupted_runs +
//! rebuild_and_reconcile_all → register all commands → forward the event bus
//! to the frontend as `buddy:event` emits → setup_menu → init_updater.

pub mod buddy;
pub mod commands;
pub mod menu;
pub mod updater;

use tauri::{Emitter, Manager};

use buddy::events::BuddyEventBus;
use buddy::notifications::create_task_notifier;
use buddy::service::{BuddyCoreService, BuddyCoreServiceOptions};

/// Builds and runs the Tauri application.
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    buddy::shell_path::fix_shell_path();

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        // TS window-manager.ts: show the window on `ready-to-show`.
        .on_page_load(|window, _payload| {
            let _ = window.show();
        })
        .setup(|app| {
            let app_handle = app.handle().clone();

            // Core service: store + runner + queue coordinator + event bus
            // (TS: `new BuddyCoreService({ events: buddyEvents })`).
            let events = BuddyEventBus::new();
            let notifier_app = app_handle.clone();
            let service = BuddyCoreService::new(BuddyCoreServiceOptions {
                events: Some(events.clone()),
                notifier_factory: Some(Box::new(move |store| {
                    create_task_notifier(notifier_app.clone(), store)
                })),
                ..Default::default()
            });

            // Forward the event bus to the frontend (TS:
            // `buddyEvents.subscribe(e => win.webContents.send('buddy:event', e))`).
            // High-frequency `actor.stdout` chunks are coalesced for a short
            // window so each emit does not become a separate JS task on the
            // webview main thread (which made typing stutter during runs).
            let mut receiver = events.subscribe();
            let emitter = app_handle.clone();
            tauri::async_runtime::spawn(async move {
                let mut coalescer = buddy::coalesce::StdoutCoalescer::new();
                loop {
                    if coalescer.has_pending() {
                        tokio::select! {
                            recv = receiver.recv() => {
                                match recv {
                                    Ok(envelope) => {
                                        for ready in coalescer.push(envelope) {
                                            let _ = emitter.emit("buddy:event", ready);
                                        }
                                    }
                                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                        if let Some(pending) = coalescer.take_pending() {
                                            let _ = emitter.emit("buddy:event", pending);
                                        }
                                        break;
                                    }
                                }
                            }
                            _ = tokio::time::sleep(buddy::coalesce::COALESCE_WINDOW) => {
                                if let Some(pending) = coalescer.take_pending() {
                                    let _ = emitter.emit("buddy:event", pending);
                                }
                            }
                        }
                    } else {
                        match receiver.recv().await {
                            Ok(envelope) => {
                                for ready in coalescer.push(envelope) {
                                    let _ = emitter.emit("buddy:event", ready);
                                }
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        }
                    }
                }
            });

            // Startup recovery + queue rebuild. The Electron edition awaits
            // this in `app.whenReady()` before creating the window; Tauri's
            // setup hook runs before the configured windows are created, so
            // blocking here preserves that ordering.
            if let Err(error) =
                tauri::async_runtime::block_on(service.recover_interrupted_runs())
            {
                eprintln!("[buddy] startup recovery failed: {error}");
            }

            app.manage(service);

            menu::setup_menu(app.handle())?;
            updater::init_updater(app.handle());

            // Forward fullscreen transitions to the frontend (TS:
            // window-manager.ts `enter-full-screen`/`leave-full-screen` →
            // `window:fullScreenChange`). Tauri has no dedicated fullscreen
            // event, so poll the state on window resize.
            if let Some(window) = app.get_webview_window("main") {
                let _fullscreen_emitter = app_handle.clone();
                let was_fullscreen = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(
                    window.is_fullscreen().unwrap_or(false),
                ));
                window.on_window_event(move |event| {
                    if let tauri::WindowEvent::Resized(_) = event {
                        if let Some(win) =
                            _fullscreen_emitter.get_webview_window("main")
                        {
                            let is_fullscreen = win.is_fullscreen().unwrap_or(false);
                            if was_fullscreen
                                .swap(is_fullscreen, std::sync::atomic::Ordering::Relaxed)
                                != is_fullscreen
                            {
                                let _ = win.emit("window:fullScreenChange", is_fullscreen);
                            }
                        }
                    }
                });
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::buddy_check_health,
            commands::buddy_bootstrap,
            commands::buddy_get_tasks,
            commands::buddy_get_task_detail,
            commands::buddy_create_task,
            commands::buddy_delete_task,
            commands::buddy_start_task,
            commands::buddy_send_message,
            commands::buddy_skip_countdown,
            commands::buddy_pause_countdown,
            commands::buddy_interrupt,
            commands::buddy_enqueue_instruction,
            commands::buddy_dequeue_instruction,
            commands::buddy_clear_instruction_queue,
            commands::buddy_interrupt_and_insert,
            commands::buddy_get_events,
            commands::buddy_get_round_events,
            commands::buddy_get_task_stats,
            commands::buddy_update_global_settings,
            commands::buddy_git_status,
            commands::buddy_git_stage_all,
            commands::buddy_git_stage_files,
            commands::buddy_git_commit_and_push,
            commands::buddy_git_diff_for_commit_message,
            commands::buddy_git_file_diff,
            commands::buddy_git_branches,
            commands::buddy_git_checkout,
            commands::buddy_git_create_branch,
            commands::buddy_generate_commit_message,
            commands::buddy_cancel_generate_commit_message,
            commands::buddy_test_launcher,
            commands::buddy_detect_actor_models,
            commands::buddy_update_task_text,
            commands::read_clipboard_file_paths,
            commands::save_attachment_buffer,
            commands::read_file_as_data_url,
            commands::update_menu_language,
            commands::updater_check,
            commands::updater_download,
            commands::updater_install,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Buddy application");
}
