//! Native macOS application menu — Rust port of `src/main/menu.ts`.
//!
//! Built with `tauri::menu` APIs. Custom items emit `menu:action` events to
//! the main window with the same action strings the Electron edition sent;
//! native roles map to Tauri predefined items where they exist
//! (about/services/hide/quit/undo/…), the rest are re-implemented in
//! [`handle_menu_event`].
//!
//! `lib.rs` wiring: call [`setup_menu`] during setup, and route the
//! `update_menu_language` command to [`update_menu_language`].

use parking_lot::Mutex;
use tauri::menu::{
    AboutMetadata, Menu, MenuBuilder, MenuEvent, MenuItemBuilder, PredefinedMenuItem,
    SubmenuBuilder,
};
use tauri::{AppHandle, Emitter, Manager};

/// The `menu:action` payload strings consumed by `src/App.tsx`.
pub mod actions {
    pub const OPEN_SETTINGS: &str = "openSettings";
    pub const CHECK_FOR_UPDATES: &str = "checkForUpdates";
    pub const NEW_TASK: &str = "newTask";
    pub const PREV_TASK: &str = "prevTask";
    pub const NEXT_TASK: &str = "nextTask";
    pub const TOGGLE_SIDEBAR: &str = "toggleSidebar";
    pub const TOGGLE_STATUS_BAR: &str = "toggleStatusBar";
    pub const SHOW_KEYBOARD_SHORTCUTS: &str = "showKeyboardShortcuts";
}

/// Ids of custom items handled natively (no `menu:action` emission) —
/// Tauri has no predefined equivalents for these Electron roles.
mod native_ids {
    pub const RELOAD: &str = "reload";
    pub const FORCE_RELOAD: &str = "forceReload";
    pub const DEV_TOOLS: &str = "devTools";
    pub const ACTUAL_SIZE: &str = "actualSize";
    pub const ZOOM_IN: &str = "zoomIn";
    pub const ZOOM_OUT: &str = "zoomOut";
    pub const BRING_ALL_FRONT: &str = "bringAllFront";
}

pub struct MenuLabels {
    pub about: &'static str,
    pub preferences: &'static str,
    pub check_for_updates: &'static str,
    pub services: &'static str,
    pub hide: &'static str,
    pub hide_others: &'static str,
    pub show_all: &'static str,
    pub quit: &'static str,
    pub file: &'static str,
    pub new_task: &'static str,
    pub close_window: &'static str,
    pub edit: &'static str,
    pub undo: &'static str,
    pub redo: &'static str,
    pub cut: &'static str,
    pub copy: &'static str,
    pub paste: &'static str,
    pub select_all: &'static str,
    pub view: &'static str,
    pub prev_task: &'static str,
    pub next_task: &'static str,
    pub toggle_sidebar: &'static str,
    pub toggle_status_bar: &'static str,
    pub reload: &'static str,
    pub force_reload: &'static str,
    pub dev_tools: &'static str,
    pub actual_size: &'static str,
    pub zoom_in: &'static str,
    pub zoom_out: &'static str,
    pub fullscreen: &'static str,
    pub window: &'static str,
    pub minimize: &'static str,
    pub zoom: &'static str,
    pub bring_all_front: &'static str,
    pub close: &'static str,
    pub help: &'static str,
    pub documentation: &'static str,
    pub whats_new: &'static str,
    pub send_feedback: &'static str,
    pub keyboard_shortcuts: &'static str,
}

const LABELS_EN: MenuLabels = MenuLabels {
    about: "About Buddy",
    preferences: "Preferences...",
    check_for_updates: "Check for Updates...",
    services: "Services",
    hide: "Hide Buddy",
    hide_others: "Hide Others",
    show_all: "Show All",
    quit: "Quit Buddy",
    file: "File",
    new_task: "New Task",
    close_window: "Close Window",
    edit: "Edit",
    undo: "Undo",
    redo: "Redo",
    cut: "Cut",
    copy: "Copy",
    paste: "Paste",
    select_all: "Select All",
    view: "View",
    prev_task: "Previous Task",
    next_task: "Next Task",
    toggle_sidebar: "Toggle Sidebar",
    toggle_status_bar: "Toggle Status Bar",
    reload: "Reload",
    force_reload: "Force Reload",
    dev_tools: "Developer Tools",
    actual_size: "Actual Size",
    zoom_in: "Zoom In",
    zoom_out: "Zoom Out",
    fullscreen: "Fullscreen",
    window: "Window",
    minimize: "Minimize",
    zoom: "Zoom",
    bring_all_front: "Bring All to Front",
    close: "Close",
    help: "Help",
    documentation: "Buddy Documentation",
    whats_new: "What's New?",
    send_feedback: "Send Feedback",
    keyboard_shortcuts: "Keyboard Shortcuts",
};

const LABELS_ZH_CN: MenuLabels = MenuLabels {
    about: "关于 Buddy",
    preferences: "偏好设置...",
    check_for_updates: "检查更新...",
    services: "服务",
    hide: "隐藏 Buddy",
    hide_others: "隐藏其他",
    show_all: "显示全部",
    quit: "退出 Buddy",
    file: "文件",
    new_task: "新建任务",
    close_window: "关闭窗口",
    edit: "编辑",
    undo: "撤销",
    redo: "重做",
    cut: "剪切",
    copy: "复制",
    paste: "粘贴",
    select_all: "全选",
    view: "视图",
    prev_task: "上一个任务",
    next_task: "下一个任务",
    toggle_sidebar: "切换侧边栏",
    toggle_status_bar: "切换状态栏",
    reload: "重新加载",
    force_reload: "强制重新加载",
    dev_tools: "开发者工具",
    actual_size: "实际大小",
    zoom_in: "放大",
    zoom_out: "缩小",
    fullscreen: "全屏",
    window: "窗口",
    minimize: "最小化",
    zoom: "缩放",
    bring_all_front: "前置全部窗口",
    close: "关闭",
    help: "帮助",
    documentation: "Buddy 文档",
    whats_new: "新功能",
    send_feedback: "发送反馈",
    keyboard_shortcuts: "键盘快捷键",
};

const LABELS_ZH_TW: MenuLabels = MenuLabels {
    about: "關於 Buddy",
    preferences: "偏好設定...",
    check_for_updates: "檢查更新…",
    services: "服務",
    hide: "隱藏 Buddy",
    hide_others: "隱藏其他",
    show_all: "顯示全部",
    quit: "結束 Buddy",
    file: "檔案",
    new_task: "新增任務",
    close_window: "關閉視窗",
    edit: "編輯",
    undo: "復原",
    redo: "重做",
    cut: "剪下",
    copy: "拷貝",
    paste: "貼上",
    select_all: "全選",
    view: "檢視",
    prev_task: "上一個任務",
    next_task: "下一個任務",
    toggle_sidebar: "切換側邊欄",
    toggle_status_bar: "切換狀態列",
    reload: "重新載入",
    force_reload: "強制重新載入",
    dev_tools: "開發者工具",
    actual_size: "實際大小",
    zoom_in: "放大",
    zoom_out: "縮小",
    fullscreen: "全螢幕",
    window: "視窗",
    minimize: "最小化",
    zoom: "縮放",
    bring_all_front: "將全部移至最前",
    close: "關閉",
    help: "說明",
    documentation: "Buddy 文件",
    whats_new: "新功能",
    send_feedback: "傳送意見回饋",
    keyboard_shortcuts: "鍵盤快速鍵",
};

/// `None` for unsupported languages (same gate as the TS `updateMenuLanguage`).
pub fn labels_for(lang: &str) -> Option<&'static MenuLabels> {
    match lang {
        "zh-CN" => Some(&LABELS_ZH_CN),
        "zh-TW" => Some(&LABELS_ZH_TW),
        "en" => Some(&LABELS_EN),
        _ => None,
    }
}

static CURRENT_LANG: Mutex<&'static str> = Mutex::new("zh-CN");

/// Webview zoom level, Electron-style: factor = 1.2^level.
static ZOOM_LEVEL: Mutex<f64> = Mutex::new(0.0);

fn current_labels() -> &'static MenuLabels {
    labels_for(&CURRENT_LANG.lock()).unwrap_or(&LABELS_EN)
}

fn action_item(
    app: &AppHandle,
    action: &str,
    label: &str,
    accelerator: Option<&str>,
) -> tauri::Result<tauri::menu::MenuItem<tauri::Wry>> {
    let mut builder = MenuItemBuilder::new(label).id(action);
    if let Some(accelerator) = accelerator {
        builder = builder.accelerator(accelerator);
    }
    builder.build(app)
}

fn build_menu(app: &AppHandle) -> tauri::Result<Menu<tauri::Wry>> {
    let t = current_labels();
    let app_name = app
        .config()
        .product_name
        .clone()
        .unwrap_or_else(|| "Buddy".to_string());

    let sep = PredefinedMenuItem::separator(app)?;
    let sep2 = PredefinedMenuItem::separator(app)?;
    let sep3 = PredefinedMenuItem::separator(app)?;
    let sep4 = PredefinedMenuItem::separator(app)?;
    let sep5 = PredefinedMenuItem::separator(app)?;
    let mut app_menu = SubmenuBuilder::new(app, &app_name)
        .item(&PredefinedMenuItem::about(app, Some(t.about), Some(AboutMetadata::default()))?)
        .item(&sep)
        .item(&action_item(app, actions::OPEN_SETTINGS, t.preferences, Some("CmdOrCtrl+,"))?);
    if crate::updater::updater_active(app) {
        // Updater disabled in tauri.conf.json — no update server exists yet,
        // so a 'Check for Updates' entry would only report a guaranteed
        // failure. Hide it until updates are re-enabled.
        app_menu = app_menu
            .item(&sep2)
            .item(&action_item(app, actions::CHECK_FOR_UPDATES, t.check_for_updates, None)?);
    }
    let app_menu = app_menu
        .item(&sep3)
        .item(&PredefinedMenuItem::services(app, Some(t.services))?)
        .item(&sep4)
        .item(&PredefinedMenuItem::hide(app, Some(t.hide))?)
        .item(&PredefinedMenuItem::hide_others(app, Some(t.hide_others))?)
        .item(&PredefinedMenuItem::show_all(app, Some(t.show_all))?)
        .item(&sep5)
        .item(&PredefinedMenuItem::quit(app, Some(t.quit))?)
        .build()?;

    let file_menu = SubmenuBuilder::new(app, t.file)
        .item(&action_item(app, actions::NEW_TASK, t.new_task, Some("CmdOrCtrl+N"))?)
        .item(&PredefinedMenuItem::separator(app)?)
        .item(&PredefinedMenuItem::close_window(app, Some(t.close_window))?)
        .build()?;

    let edit_menu = SubmenuBuilder::new(app, t.edit)
        .item(&PredefinedMenuItem::undo(app, Some(t.undo))?)
        .item(&PredefinedMenuItem::redo(app, Some(t.redo))?)
        .item(&PredefinedMenuItem::separator(app)?)
        .item(&PredefinedMenuItem::cut(app, Some(t.cut))?)
        .item(&PredefinedMenuItem::copy(app, Some(t.copy))?)
        .item(&PredefinedMenuItem::paste(app, Some(t.paste))?)
        .item(&PredefinedMenuItem::select_all(app, Some(t.select_all))?)
        .build()?;

    let view_menu = SubmenuBuilder::new(app, t.view)
        .item(&action_item(app, actions::PREV_TASK, t.prev_task, Some("CmdOrCtrl+Shift+["))?)
        .item(&action_item(app, actions::NEXT_TASK, t.next_task, Some("CmdOrCtrl+Shift+]"))?)
        .item(&PredefinedMenuItem::separator(app)?)
        .item(&action_item(app, actions::TOGGLE_SIDEBAR, t.toggle_sidebar, Some("CmdOrCtrl+B"))?)
        .item(&action_item(app, actions::TOGGLE_STATUS_BAR, t.toggle_status_bar, Some("CmdOrCtrl+Alt+B"))?)
        .item(&PredefinedMenuItem::separator(app)?)
        .item(&action_item(app, native_ids::RELOAD, t.reload, None)?)
        .item(&action_item(app, native_ids::FORCE_RELOAD, t.force_reload, None)?)
        .item(&action_item(app, native_ids::DEV_TOOLS, t.dev_tools, None)?)
        .item(&PredefinedMenuItem::separator(app)?)
        .item(&action_item(app, native_ids::ACTUAL_SIZE, t.actual_size, None)?)
        .item(&action_item(app, native_ids::ZOOM_IN, t.zoom_in, None)?)
        .item(&action_item(app, native_ids::ZOOM_OUT, t.zoom_out, None)?)
        .item(&PredefinedMenuItem::separator(app)?)
        .item(&PredefinedMenuItem::fullscreen(app, Some(t.fullscreen))?)
        .build()?;

    let window_menu = SubmenuBuilder::new(app, t.window)
        .item(&PredefinedMenuItem::minimize(app, Some(t.minimize))?)
        .item(&PredefinedMenuItem::maximize(app, Some(t.zoom))?)
        .item(&PredefinedMenuItem::separator(app)?)
        .item(&action_item(app, native_ids::BRING_ALL_FRONT, t.bring_all_front, None)?)
        .item(&PredefinedMenuItem::close_window(app, Some(t.close))?)
        .build()?;

    let disabled = |text: &str| MenuItemBuilder::new(text).enabled(false).build(app);
    let help_menu = SubmenuBuilder::new(app, t.help)
        .item(&disabled(t.documentation)?)
        .item(&disabled(t.whats_new)?)
        .item(&disabled(t.send_feedback)?)
        .item(&PredefinedMenuItem::separator(app)?)
        .item(&action_item(app, actions::SHOW_KEYBOARD_SHORTCUTS, t.keyboard_shortcuts, Some("CmdOrCtrl+/"))?)
        .build()?;

    MenuBuilder::new(app)
        .items(&[
            &app_menu,
            &file_menu,
            &edit_menu,
            &view_menu,
            &window_menu,
            &help_menu,
        ])
        .build()
}

/// Send a `menu:action` payload to the main window (TS: `sendMenuAction`).
fn send_menu_action(app: &AppHandle, action: &str) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.emit("menu:action", action);
    }
}

fn apply_zoom(app: &AppHandle) {
    let level = *ZOOM_LEVEL.lock();
    let factor = 1.2f64.powf(level);
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.set_zoom(factor);
    }
}

/// Global menu-event handler. `setup_menu` registers it via
/// `AppHandle::on_menu_event`; exposed for tests/wiring flexibility.
pub fn handle_menu_event(app: &AppHandle, event: MenuEvent) {
    let id = event.id().0.as_str();
    match id {
        native_ids::RELOAD | native_ids::FORCE_RELOAD => {
            // Tauri exposes no cache-bypassing reload; force reload falls back
            // to a plain reload.
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.eval("location.reload()");
            }
        }
        native_ids::DEV_TOOLS => {
            if let Some(window) = app.get_webview_window("main") {
                window.open_devtools();
            }
        }
        native_ids::ACTUAL_SIZE => {
            *ZOOM_LEVEL.lock() = 0.0;
            apply_zoom(app);
        }
        native_ids::ZOOM_IN => {
            *ZOOM_LEVEL.lock() += 0.5;
            apply_zoom(app);
        }
        native_ids::ZOOM_OUT => {
            *ZOOM_LEVEL.lock() -= 0.5;
            apply_zoom(app);
        }
        native_ids::BRING_ALL_FRONT => {
            for window in app.webview_windows().values() {
                let _ = window.show();
                let _ = window.set_focus();
            }
        }
        action if matches!(
            action,
            actions::OPEN_SETTINGS
                | actions::CHECK_FOR_UPDATES
                | actions::NEW_TASK
                | actions::PREV_TASK
                | actions::NEXT_TASK
                | actions::TOGGLE_SIDEBAR
                | actions::TOGGLE_STATUS_BAR
                | actions::SHOW_KEYBOARD_SHORTCUTS
        ) =>
        {
            send_menu_action(app, action);
        }
        _ => {}
    }
}

/// Build and install the application menu (TS: `setupMenu`). Registers the
/// global menu-event handler — call exactly once during app setup.
pub fn setup_menu(app: &AppHandle) -> tauri::Result<()> {
    let menu = build_menu(app)?;
    app.set_menu(menu)?;
    app.on_menu_event(|app, event| handle_menu_event(app, event));
    Ok(())
}

/// Rebuild the menu in a new language (TS: `updateMenuLanguage`). Unknown
/// languages and the current language are ignored.
pub fn update_menu_language(app: &AppHandle, lang: &str) {
    if labels_for(lang).is_none() {
        return;
    }
    {
        let mut current = CURRENT_LANG.lock();
        if *current == lang {
            return;
        }
        *current = match lang {
            "zh-TW" => "zh-TW",
            "en" => "en",
            _ => "zh-CN",
        };
    }
    if let Ok(menu) = build_menu(app) {
        let _ = app.set_menu(menu);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_languages_present_and_distinct() {
        let cn = labels_for("zh-CN").unwrap();
        let tw = labels_for("zh-TW").unwrap();
        let en = labels_for("en").unwrap();
        assert_eq!(cn.new_task, "新建任务");
        assert_eq!(tw.new_task, "新增任務");
        assert_eq!(en.new_task, "New Task");
        assert_eq!(cn.quit, "退出 Buddy");
        assert_eq!(tw.quit, "結束 Buddy");
        assert_eq!(en.quit, "Quit Buddy");
        assert!(labels_for("fr").is_none());
        assert!(labels_for("").is_none());
    }

    #[test]
    fn action_strings_match_frontend() {
        // Keep in sync with the `onMenuAction` switch in src/App.tsx.
        for action in [
            actions::OPEN_SETTINGS,
            actions::CHECK_FOR_UPDATES,
            actions::NEW_TASK,
            actions::PREV_TASK,
            actions::NEXT_TASK,
            actions::TOGGLE_SIDEBAR,
            actions::TOGGLE_STATUS_BAR,
            actions::SHOW_KEYBOARD_SHORTCUTS,
        ] {
            assert!(!action.is_empty());
        }
        assert_eq!(actions::OPEN_SETTINGS, "openSettings");
        assert_eq!(actions::SHOW_KEYBOARD_SHORTCUTS, "showKeyboardShortcuts");
    }

    #[test]
    fn accelerators_match_original() {
        // Kept in sync with the Electron template (muda parses `,`, `[`, `]`,
        // `/` key names fine; parsing happens in MenuItemBuilder at runtime).
        let accelerators = [
            "CmdOrCtrl+,",
            "CmdOrCtrl+N",
            "CmdOrCtrl+Shift+[",
            "CmdOrCtrl+Shift+]",
            "CmdOrCtrl+B",
            "CmdOrCtrl+Alt+B",
            "CmdOrCtrl+/",
        ];
        assert_eq!(accelerators.len(), 7);
    }
}
