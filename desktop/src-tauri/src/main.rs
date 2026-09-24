//! DeepTutor desktop shell.
//!
//! Phase 1 shape: a supervisor thread owns the Python launcher, the window is
//! handed over to the loopback UI once the launcher reports ready, and every
//! command the UI may call lives in `tauri-plugin-deeptutor` because Tauri's
//! ACL only grants *plugin* commands to a remote origin (Phase 0 finding).

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod runtime_info;
mod supervisor;

use std::sync::Arc;

use tauri::{Manager, RunEvent};
use tauri_plugin_deeptutor::DesktopBackend;

use supervisor::{ShellConfig, Supervisor};

fn main() {
    // Headless smoke test for CI: resolve the shell configuration and print it
    // without starting the launcher or opening a window, so it also works on a
    // runner that has no display.
    if std::env::args().any(|arg| arg == "--self-check") {
        let config = ShellConfig::resolve();
        let payload = serde_json::json!({
            "shell": "deeptutor-desktop",
            "mode": "self-check",
            "home": config.home,
            "workdir": config.workdir,
            "python": config.python,
            "state_path": config.state_path,
            "logs_dir": config.logs_dir,
            "python_exists": config.python.exists(),
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "{}".to_string())
        );
        return;
    }

    let config = ShellConfig::resolve();
    let supervisor = Supervisor::new_shared(config);
    let backend: Arc<dyn DesktopBackend> = supervisor.clone();

    tauri::Builder::default()
        // Single instance is registered first: it decides whether this process
        // is the one that owns the app or just focuses the existing window.
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            app::reveal(app);
        }))
        .plugin(tauri_plugin_window_state::Builder::default().build())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_deeptutor::init(backend))
        .setup({
            let supervisor = Arc::clone(&supervisor);
            move |app| {
                app.manage(Arc::clone(&supervisor));
                let handle = app.handle().clone();
                supervisor.attach_app(handle.clone());
                let window = app
                    .get_webview_window("main")
                    .ok_or_else(|| "主窗口未在 tauri.conf.json 中声明".to_string())?;
                supervisor.attach_window(window);
                app::install_menu(&handle)?;
                app::install_tray(&handle)?;
                app.on_menu_event(|handle, event| {
                    app::on_menu_event(handle, event.id().as_ref());
                });
                supervisor.start();
                Ok(())
            }
        })
        .build(tauri::generate_context!())
        .expect("failed to build the DeepTutor desktop shell")
        .run(|app_handle, event| {
            if let RunEvent::Exit = event {
                // The launcher also watches this PID, but a normal quit can
                // stop its two children immediately instead of within 2s.
                app_handle.state::<Arc<Supervisor>>().stop();
            }
        });
}
