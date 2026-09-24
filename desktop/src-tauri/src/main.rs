//! DeepTutor desktop shell — Phase 0 skeleton.
//!
//! Phase 0 goal: prove that a Tauri window can boot the existing Python
//! launcher, wait for its `--runtime-info` handshake, and hand the window over
//! to the loopback Next.js server — without touching the Web or CLI path.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod runtime_info;
mod supervisor;

use std::sync::Arc;

use tauri::{Manager, RunEvent};

use supervisor::{ShellConfig, Supervisor};

/// Remote-IPC self-test: proves the locally served UI can call the shell.
/// The caller's URL is logged so the splash (app origin) and the real UI
/// (loopback origin) are distinguishable in `desktop/logs/shell.log`.
#[tauri::command]
fn desktop_probe(
    window: tauri::WebviewWindow,
    supervisor: tauri::State<'_, Arc<Supervisor>>,
) -> serde_json::Value {
    let origin = window
        .url()
        .map(|url| url.to_string())
        .unwrap_or_else(|_| "<unknown>".to_string());
    supervisor.record_probe(&origin);
    supervisor.probe()
}

fn main() {
    let config = ShellConfig::resolve();

    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![desktop_probe])
        .setup(move |app| {
            let supervisor = Arc::new(Supervisor::new(config));
            app.manage(Arc::clone(&supervisor));
            let window = app
                .get_webview_window("main")
                .ok_or_else(|| "主窗口未在 tauri.conf.json 中声明".to_string())?;
            supervisor.attach_window(window);
            supervisor.start();
            Ok(())
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
