//! Native chrome: menu bar, tray icon and the actions they trigger.
//!
//! Everything a user can reach from outside the web page lives here, so the
//! window contents stay the web app's business and this file stays a list of
//! "what the OS offers a desktop user".

use std::sync::Arc;

use tauri::menu::{
    AboutMetadata, Menu, MenuBuilder, MenuItemBuilder, PredefinedMenuItem, SubmenuBuilder,
};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, Runtime};

use crate::supervisor::Supervisor;

/// Menu ids, kept in one place so the handler cannot drift from the builder.
pub const MENU_RESTART_SERVICE: &str = "restart_service";
pub const MENU_OPEN_LOGS: &str = "open_logs";
pub const MENU_OPEN_SETTINGS: &str = "open_settings";
pub const MENU_RELOAD_UI: &str = "reload_ui";
pub const MENU_OPEN_DOCS: &str = "open_docs";
pub const TRAY_SHOW: &str = "tray_show";
pub const TRAY_QUIT: &str = "tray_quit";

const DOCS_URL: &str = "https://github.com/HKUDS/DeepTutor#readme";
const MAIN_WINDOW: &str = "main";

/// Build and install the menu bar. Every item is bound to a local first: a
/// menu builder wants `&dyn IsMenuItem`, and inline `&Foo(app)?` temporaries do
/// not always live long enough inside an array.
pub fn install_menu<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<()> {
    let settings = MenuItemBuilder::with_id(MENU_OPEN_SETTINGS, "设置 …")
        .accelerator("CmdOrCtrl+,")
        .build(app)?;
    let restart = MenuItemBuilder::with_id(MENU_RESTART_SERVICE, "重新启动本地服务").build(app)?;
    let open_logs = MenuItemBuilder::with_id(MENU_OPEN_LOGS, "打开日志目录").build(app)?;
    let docs = MenuItemBuilder::with_id(MENU_OPEN_DOCS, "使用文档").build(app)?;
    let reload = MenuItemBuilder::with_id(MENU_RELOAD_UI, "重新加载界面")
        .accelerator("CmdOrCtrl+R")
        .build(app)?;

    let about =
        PredefinedMenuItem::about(app, Some("关于 DeepTutor"), Some(AboutMetadata::default()))?;
    let services = PredefinedMenuItem::services(app, None)?;
    let hide = PredefinedMenuItem::hide(app, None)?;
    let hide_others = PredefinedMenuItem::hide_others(app, None)?;
    let show_all = PredefinedMenuItem::show_all(app, None)?;
    let quit = PredefinedMenuItem::quit(app, Some("退出 DeepTutor"))?;

    let app_sep1 = PredefinedMenuItem::separator(app)?;
    let app_sep2 = PredefinedMenuItem::separator(app)?;
    let app_sep3 = PredefinedMenuItem::separator(app)?;
    let app_sep4 = PredefinedMenuItem::separator(app)?;
    let app_menu = SubmenuBuilder::new(app, "DeepTutor")
        .items(&[
            &about,
            &app_sep1,
            &settings,
            &restart,
            &open_logs,
            &app_sep2,
            &services,
            &app_sep3,
            &hide,
            &hide_others,
            &show_all,
            &app_sep4,
            &quit,
        ])
        .build()?;

    // The Edit menu is what makes ⌘C/⌘V work inside the webview at all.
    let undo = PredefinedMenuItem::undo(app, None)?;
    let redo = PredefinedMenuItem::redo(app, None)?;
    let cut = PredefinedMenuItem::cut(app, None)?;
    let copy = PredefinedMenuItem::copy(app, None)?;
    let paste = PredefinedMenuItem::paste(app, None)?;
    let select_all = PredefinedMenuItem::select_all(app, None)?;
    let edit_sep = PredefinedMenuItem::separator(app)?;
    let edit_menu = SubmenuBuilder::new(app, "编辑")
        .items(&[&undo, &redo, &edit_sep, &cut, &copy, &paste, &select_all])
        .build()?;

    let fullscreen = PredefinedMenuItem::fullscreen(app, None)?;
    #[cfg(debug_assertions)]
    let view_menu = {
        let sep = PredefinedMenuItem::separator(app)?;
        let devtools = MenuItemBuilder::with_id("toggle_devtools", "开发者工具")
            .accelerator("Alt+CmdOrCtrl+I")
            .build(app)?;
        SubmenuBuilder::new(app, "视图")
            .items(&[&reload, &fullscreen, &sep, &devtools])
            .build()?
    };
    #[cfg(not(debug_assertions))]
    let view_menu = SubmenuBuilder::new(app, "视图")
        .items(&[&reload, &fullscreen])
        .build()?;

    let minimize = PredefinedMenuItem::minimize(app, None)?;
    let maximize = PredefinedMenuItem::maximize(app, None)?;
    let close_window = PredefinedMenuItem::close_window(app, None)?;
    let window_sep = PredefinedMenuItem::separator(app)?;
    let window_menu = SubmenuBuilder::new(app, "窗口")
        .items(&[&minimize, &maximize, &window_sep, &close_window])
        .build()?;

    let help_menu = SubmenuBuilder::new(app, "帮助").items(&[&docs]).build()?;

    let menu = MenuBuilder::new(app)
        .items(&[&app_menu, &edit_menu, &view_menu, &window_menu, &help_menu])
        .build()?;
    app.set_menu(menu)?;
    Ok(())
}

/// Tray icon: the app keeps working while the window is hidden.
pub fn install_tray<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<()> {
    let show = MenuItemBuilder::with_id(TRAY_SHOW, "显示主窗口").build(app)?;
    let restart = MenuItemBuilder::with_id(MENU_RESTART_SERVICE, "重新启动本地服务").build(app)?;
    let open_logs = MenuItemBuilder::with_id(MENU_OPEN_LOGS, "打开日志目录").build(app)?;
    let quit = MenuItemBuilder::with_id(TRAY_QUIT, "退出 DeepTutor").build(app)?;
    let sep1 = PredefinedMenuItem::separator(app)?;
    let sep2 = PredefinedMenuItem::separator(app)?;
    let menu: Menu<R> = Menu::with_items(app, &[&show, &restart, &sep1, &open_logs, &sep2, &quit])?;

    let mut builder = TrayIconBuilder::with_id("deeptutor")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                reveal(tray.app_handle());
            }
        });
    // A tray without an icon is invisible on macOS; the bundle icon comes from
    // `generate_context!`, so this only matters in stripped-down builds.
    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }
    builder.build(app)?;
    Ok(())
}

/// Handle every menu and tray id in one place.
pub fn on_menu_event<R: Runtime>(app: &AppHandle<R>, id: &str) {
    let supervisor = app.state::<Arc<Supervisor>>();
    match id {
        MENU_RESTART_SERVICE => {
            if let Err(error) = supervisor.restart() {
                supervisor.append_shell_log(&format!("restart failed: {error}"));
            }
            reveal(app);
        }
        MENU_OPEN_LOGS => {
            if let Err(error) = tauri_plugin_opener::open_path(supervisor.logs_dir(), None::<&str>)
            {
                supervisor.append_shell_log(&format!("open logs failed: {error}"));
            }
        }
        MENU_OPEN_SETTINGS => navigate(app, "/settings"),
        MENU_RELOAD_UI => {
            if let Some(window) = app.get_webview_window(MAIN_WINDOW) {
                let _ = window.eval("window.location.reload()");
            }
        }
        MENU_OPEN_DOCS => {
            if let Err(error) = tauri_plugin_opener::open_url(DOCS_URL, None::<&str>) {
                supervisor.append_shell_log(&format!("open docs failed: {error}"));
            }
        }
        #[cfg(debug_assertions)]
        "toggle_devtools" => {
            if let Some(window) = app.get_webview_window(MAIN_WINDOW) {
                window.open_devtools();
            }
        }
        TRAY_SHOW => reveal(app),
        TRAY_QUIT => {
            supervisor.stop();
            app.exit(0);
        }
        _ => {}
    }
}

/// Bring the main window back from hidden or minimised.
pub fn reveal<R: Runtime>(app: &AppHandle<R>) {
    if let Some(window) = app.get_webview_window(MAIN_WINDOW) {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

/// Ask the web UI to open one of its own routes.
fn navigate<R: Runtime>(app: &AppHandle<R>, route: &str) {
    if let Some(window) = app.get_webview_window(MAIN_WINDOW) {
        let _ = window.eval(format!(
            "window.location.assign({});",
            serde_json::to_string(route).unwrap_or_else(|_| "\"/\"".to_string())
        ));
    }
}
