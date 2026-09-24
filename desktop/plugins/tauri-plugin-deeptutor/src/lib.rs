//! Shell-side commands for the DeepTutor desktop UI.
//!
//! The UI is served by the loopback Next.js server, i.e. it runs on a *remote*
//! origin as far as Tauri is concerned. Phase 0 measured the consequence: core
//! and plugin commands are reachable from that origin (given a capability with
//! `remote.urls`), but application commands are not — Tauri answers
//! `desktop_probe not allowed. Plugin not found` because they carry no ACL entry.
//!
//! So every shell capability the UI must call is exposed here, as a plugin
//! command with a permission the app capability can grant.

use std::sync::Arc;

use serde::Serialize;
use tauri::plugin::{Builder, TauriPlugin};
use tauri::{Manager, Runtime, State};

/// What the shell has to provide; implemented by the shell crate itself so this
/// plugin stays free of supervisor internals (and of a dependency cycle).
pub trait DesktopBackend: Send + Sync + 'static {
    /// Snapshot of the shell and launcher state, safe to show in the UI.
    fn status(&self) -> DesktopStatus;
    /// Restart the local backend + frontend without quitting the shell.
    fn restart(&self) -> Result<(), String>;
    /// Record which origin called in. Phase 0 leaned on this to prove the
    /// remote-URL grant actually reached the shell; keep it so a future ACL
    /// regression is visible in the log instead of silent.
    fn note_caller(&self, origin: &str);
}

/// Serialisable status payload. Field names are snake_case on purpose: the same
/// shape is written to `desktop/runtime.json`, so the UI reads one vocabulary.
#[derive(Debug, Clone, Serialize)]
pub struct DesktopStatus {
    /// Monotonic counter of launcher starts in this shell session.
    pub launch_count: u64,
    /// How many crash restarts have happened in this shell session.
    pub restarts: u64,
    /// Whether a launcher process is currently alive.
    pub launcher_running: bool,
    pub launcher_pid: Option<u32>,
    /// Human-readable last status line (progress or failure text).
    pub message: String,
    /// Deployment facts useful in bug reports.
    pub home: String,
    pub workdir: String,
    pub python: String,
    /// Active runtime pack, when the shell is running from one.
    pub pack: Option<String>,
    pub logs_dir: String,
    /// Latest `runtime.json` contents, when the launcher has written one.
    pub runtime: Option<RuntimeSnapshot>,
}

/// The subset of the launcher's `--runtime-info` file the UI cares about.
#[derive(Debug, Clone, Serialize)]
pub struct RuntimeSnapshot {
    pub schema_version: u32,
    pub status: String,
    pub frontend_url: Option<String>,
    pub backend_url: Option<String>,
    pub backend_port: Option<u16>,
    pub frontend_port: Option<u16>,
    /// The token itself stays out of the webview; the UI only needs to know
    /// that the handshake produced one.
    pub token_present: bool,
}

struct BackendState(Arc<dyn DesktopBackend>);

#[tauri::command]
fn desktop_status<R: Runtime>(
    window: tauri::WebviewWindow<R>,
    backend: State<'_, BackendState>,
) -> DesktopStatus {
    if let Ok(url) = window.url() {
        backend.0.note_caller(url.as_str());
    }
    backend.0.status()
}

#[tauri::command]
async fn restart_service(backend: State<'_, BackendState>) -> Result<(), String> {
    // A restart waits for the launcher's process group to exit (up to a few
    // seconds). Doing that on the command thread would freeze the window, so it
    // moves to the blocking pool and the UI keeps painting.
    let backend = Arc::clone(&backend.0);
    tauri::async_runtime::spawn_blocking(move || backend.restart())
        .await
        .map_err(|error| error.to_string())?
}

/// Register the plugin. The caller passes the shell implementation.
pub fn init<R: Runtime>(backend: Arc<dyn DesktopBackend>) -> TauriPlugin<R> {
    // Turbofish on both parameters: `Builder` is `Builder<R, C = ()>`, and
    // leaving `C` to inference makes the chain resolve to the default runtime.
    Builder::<R, ()>::new("deeptutor")
        .invoke_handler(tauri::generate_handler![desktop_status, restart_service])
        .setup(move |app, _api| {
            // The trait object is cheap to clone and the closure runs once.
            app.manage(BackendState(Arc::clone(&backend)));
            Ok(())
        })
        .build()
}
