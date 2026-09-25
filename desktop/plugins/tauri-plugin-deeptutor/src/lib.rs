//! Shell-side commands for the DeepTutor desktop UI.
//!
//! The UI is served by the loopback Next.js server, i.e. it runs on a *remote*
//! origin as far as Tauri is concerned. Phase 0 measured the consequence: core
//! and plugin commands are reachable from that origin (given a capability with
//! `remote.urls`), but application commands are not — Tauri answers
//! `desktop_probe not allowed. Plugin not found` because they carry no ACL entry.
//!
//! So every shell capability the UI must call is exposed here, as a plugin
//! command with a permission the app capability can grant. That rule covers
//! Phase 3's native surface too: notifications, deep-link/file handoff, native
//! pickers, "reveal in folder" and the first-run choices all arrive as plugin
//! commands instead of application commands.

use std::path::PathBuf;
use std::sync::Arc;

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use tauri::plugin::{Builder, TauriPlugin};
use tauri::{Manager, Runtime, State};
use tauri_plugin_dialog::DialogExt as _;

mod updater;

pub use updater::check as check_shell_update_async;
pub use updater::install as install_shell_update_async;
pub use updater::verify_download as verify_shell_update_download;
pub use updater::ShellUpdateInstall;

/// Largest file the UI may pull through the IPC bridge.
///
/// A file dropped on the Dock icon arrives as a *path*; the UI needs bytes to
/// reuse its own upload pipeline. Base64 over IPC costs ~1.3x the file size in
/// memory on both sides, so the bridge stops well below what the HTTP upload
/// path accepts and tells the user to pick the file from the app instead.
const MAX_LOCAL_FILE_BYTES: u64 = 64 * 1024 * 1024;

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

    /// Current shell preferences plus the paths they act on (Phase 3).
    fn settings(&self) -> ShellSettingsSnapshot;
    /// Apply a partial update and return the new snapshot.
    fn update_settings(&self, patch: SettingsPatch) -> Result<ShellSettingsSnapshot, String>;
    /// The UI reports its first paint with its own elapsed time, so the shell
    /// can log where the launch time actually went.
    fn note_ui_ready(&self, elapsed_ms: u64);
    /// Post a system notification for a finished round.
    fn notify(&self, request: NotificationRequest) -> Result<NotificationOutcome, String>;
    /// Claim the session a notification pointed at, if the user has come back.
    fn take_notification_target(&self) -> Option<NotificationTarget>;
    /// Claim a pending `deeptutor://` / file-association handoff.
    fn take_open_request(&self) -> Option<OpenRequestPayload>;
    /// What the first-run wizard needs to render itself.
    fn first_run(&self) -> FirstRunState;
    /// Persist the wizard's answers. May ask the shell to restart.
    fn apply_first_run(&self, choices: FirstRunChoices) -> Result<FirstRunOutcome, String>;
    /// Check the **runtime pack** catalog.
    ///
    /// The shell plane is the plugin's own business (see `updater.rs`): it is a
    /// plugin-native capability, available wherever the Tauri app handle is.
    fn check_runtime_updates(&self) -> RuntimeUpdateReport;
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
    /// How many system notifications went out this session.
    pub notifications_posted: u64,
    /// Where the launch time went, once the UI has painted (see `startup.rs`
    /// in the shell). `None` until then.
    pub startup: Option<StartupTimings>,
    /// Window geometry, so a support log can show whether state restoration
    /// actually took effect (see `window.rs`: Tauri 2.11 never fires the
    /// window-state plugin's `on_window_ready`, so the shell restores itself).
    pub window: Option<WindowGeometry>,
    /// Latest `runtime.json` contents, when the launcher has written one.
    pub runtime: Option<RuntimeSnapshot>,
}

/// Where the main window currently is and how big it is.
#[derive(Debug, Clone, Serialize)]
pub struct WindowGeometry {
    pub width: u32,
    pub height: u32,
    pub x: i32,
    pub y: i32,
    pub maximized: bool,
    pub fullscreen: bool,
    pub visible: bool,
}

/// Launch timings, measured in the shell so they survive a slow webview.
///
/// The interesting question for a desktop app is "how long until the user sees
/// the real UI", which spans three things the shell owns: spending up the
/// launcher, the launcher reporting ready, and the loopback page painting.
#[derive(Debug, Clone, Serialize, Default)]
pub struct StartupTimings {
    /// Shell start → launcher spawned.
    pub spawn_ms: u64,
    /// Shell start → launcher reported ready.
    pub ready_ms: u64,
    /// Shell start → the UI said it had painted.
    pub ui_ms: u64,
    /// Ready → the UI said it had painted, i.e. the part the shell can still
    /// influence (window creation, navigation, first render).
    pub ready_to_ui_ms: u64,
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

/// Shell preferences as the settings page sees them (Phase 3).
#[derive(Debug, Clone, Serialize)]
pub struct ShellSettingsSnapshot {
    pub close_to_tray: bool,
    pub notifications: bool,
    /// Empty means "leave the web UI's own language alone".
    pub locale: String,
    pub first_run_completed: bool,
    pub pack_catalog: Option<String>,
    /// The data directory in force right now.
    pub home: String,
    /// The platform default, i.e. where the wizard looks for its bootstrap file.
    pub default_home: String,
    /// True when a data-directory choice is waiting for a relaunch.
    pub restart_required: bool,
}

/// Partial settings update. Absent fields keep their current value; an empty
/// `pack_catalog` string clears the setting (JSON has no `undefined`).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SettingsPatch {
    pub close_to_tray: Option<bool>,
    pub notifications: Option<bool>,
    pub locale: Option<String>,
    pub first_run_completed: Option<bool>,
    pub pack_catalog: Option<String>,
}

/// One finished round, as the UI reports it.
#[derive(Debug, Clone, Deserialize)]
pub struct NotificationRequest {
    pub title: String,
    pub body: String,
    /// In-app route the notification should return to.
    pub route: String,
    pub session_id: Option<String>,
    /// Free-form tag (`round_complete`, `export_ready`, …) for logs and tests.
    #[serde(default)]
    pub kind: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct NotificationOutcome {
    /// False when notifications are switched off in shell settings.
    pub delivered: bool,
    pub permission: String,
    pub detail: Option<String>,
}

/// Where a notification wants to take the user.
#[derive(Debug, Clone, Serialize)]
pub struct NotificationTarget {
    pub route: String,
    pub session_id: Option<String>,
    pub title: String,
    pub age_ms: u64,
}

/// A `deeptutor://` link or a file the OS asked us to open.
#[derive(Debug, Clone, Serialize)]
pub struct OpenRequestPayload {
    /// `route` for deep links, `file` for file associations / Dock drops.
    pub kind: String,
    pub route: Option<String>,
    pub path: Option<String>,
    /// Where the request came from (`deep-link`, `file-association`, `argv`).
    pub source: String,
    /// The raw URL or argument, for logs.
    pub raw: String,
    pub age_ms: u64,
}

/// Everything the first-run wizard needs before the launcher starts.
#[derive(Debug, Clone, Serialize)]
pub struct FirstRunState {
    pub completed: bool,
    pub locale: String,
    pub default_locale: String,
    pub close_to_tray: bool,
    pub notifications: bool,
    pub home: String,
    pub default_home: String,
    pub can_change_data_dir: bool,
}

/// The wizard's answers.
#[derive(Debug, Clone, Deserialize)]
pub struct FirstRunChoices {
    pub locale: String,
    /// Absolute path, or `None`/empty to keep the platform default.
    pub data_dir: Option<String>,
    pub close_to_tray: bool,
    pub notifications: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct FirstRunOutcome {
    pub restart_required: bool,
    pub settings: ShellSettingsSnapshot,
}

#[derive(Debug, Clone, Serialize)]
pub struct RuntimeUpdateReport {
    pub checked: bool,
    pub source: Option<String>,
    pub updated: bool,
    pub active_pack: Option<String>,
    pub previous_pack: Option<String>,
    pub app_version: Option<String>,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ShellUpdateReport {
    /// `available` | `up_to_date` | `error`.
    pub status: String,
    pub detail: String,
    /// Version the update channel offers, when there is one.
    pub available_version: Option<String>,
    /// The running shell's version.
    pub current_version: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct UpdateReport {
    pub runtime: RuntimeUpdateReport,
    pub shell: ShellUpdateReport,
}

/// One native file-picker filter.
#[derive(Debug, Clone, Deserialize)]
pub struct PickFilter {
    pub name: String,
    pub extensions: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct PickOptions {
    pub title: Option<String>,
    pub filters: Vec<PickFilter>,
    #[serde(default)]
    pub multiple: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct LocalFilePayload {
    pub path: String,
    pub name: String,
    pub size: u64,
    pub mime: Option<String>,
    pub base64: String,
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

#[tauri::command]
fn shell_settings(backend: State<'_, BackendState>) -> ShellSettingsSnapshot {
    backend.0.settings()
}

#[tauri::command]
fn update_shell_settings(
    patch: SettingsPatch,
    backend: State<'_, BackendState>,
) -> Result<ShellSettingsSnapshot, String> {
    backend.0.update_settings(patch)
}

/// The UI's own "I have painted" call.
///
/// `elapsed_ms` comes from `performance.now()` in the page, which starts when
/// the document does; the shell adds the time it spent before handing the window
/// over, so the two numbers line up on one axis.
// Explicitly camelCase: the page sends `elapsedMs`, and a rename mismatch here
// fails silently as a rejected promise while the app keeps working.
#[tauri::command(rename_all = "camelCase")]
fn note_ui_ready(elapsed_ms: u64, backend: State<'_, BackendState>) {
    backend.0.note_ui_ready(elapsed_ms);
}

#[tauri::command]
fn notify_round_complete(
    request: NotificationRequest,
    backend: State<'_, BackendState>,
) -> Result<NotificationOutcome, String> {
    backend.0.notify(request)
}

#[tauri::command]
fn take_notification_target(backend: State<'_, BackendState>) -> Option<NotificationTarget> {
    backend.0.take_notification_target()
}

#[tauri::command]
fn take_open_request(backend: State<'_, BackendState>) -> Option<OpenRequestPayload> {
    backend.0.take_open_request()
}

#[tauri::command]
fn first_run_state(backend: State<'_, BackendState>) -> FirstRunState {
    backend.0.first_run()
}

#[tauri::command]
fn apply_first_run(
    choices: FirstRunChoices,
    backend: State<'_, BackendState>,
) -> Result<FirstRunOutcome, String> {
    backend.0.apply_first_run(choices)
}

#[tauri::command]
async fn check_updates<R: Runtime>(
    app: tauri::AppHandle<R>,
    backend: State<'_, BackendState>,
) -> Result<UpdateReport, String> {
    // The catalog check may download a pack; never on the command thread.
    let backend = Arc::clone(&backend.0);
    let runtime = tauri::async_runtime::spawn_blocking(move || backend.check_runtime_updates())
        .await
        .map_err(|error| error.to_string())?;
    // The shell plane is async natively, so it is awaited instead of blocking a
    // pool thread — and it must never download, only answer.
    let shell = updater::check(&app).await;
    Ok(UpdateReport { runtime, shell })
}

/// Download and install the newest shell build, then leave the restart to the
/// caller (`restart_app`) so the UI can show the outcome first.
#[tauri::command]
async fn install_shell_update<R: Runtime>(
    app: tauri::AppHandle<R>,
) -> Result<ShellUpdateInstall, String> {
    updater::install(&app).await
}

/// Shell-side log line. The UI uses this for things the webview cannot report
/// (native dialogs it opened, file handoffs it handled).
#[tauri::command]
fn log_event(message: String, backend: State<'_, BackendState>) {
    backend.0.note_caller(&format!("ui: {message}"));
}

/// Native open panel. Returns absolute paths; the caller decides how to ingest
/// them (`read_local_file` for the byte-level upload path).
#[tauri::command]
async fn pick_files<R: Runtime>(
    window: tauri::WebviewWindow<R>,
    options: Option<PickOptions>,
) -> Result<Vec<String>, String> {
    let options = options.unwrap_or_default();
    let mut builder = window.dialog().file();
    if let Some(title) = options.title {
        builder = builder.set_title(title);
    }
    for filter in &options.filters {
        let extensions: Vec<&str> = filter.extensions.iter().map(String::as_str).collect();
        builder = builder.add_filter(&filter.name, &extensions);
    }
    let picked = if options.multiple {
        builder.blocking_pick_files()
    } else {
        builder.blocking_pick_file().map(|path| vec![path])
    };
    let mut paths = Vec::new();
    for path in picked.unwrap_or_default() {
        match path.into_path() {
            Ok(path) => paths.push(path.to_string_lossy().into_owned()),
            Err(error) => return Err(format!("无法解析所选路径: {error}")),
        }
    }
    Ok(paths)
}

/// Hand a path to the OS file manager with the item selected.
#[tauri::command]
fn reveal_in_folder(path: String) -> Result<(), String> {
    tauri_plugin_opener::reveal_item_in_dir(PathBuf::from(&path))
        .map_err(|error| format!("无法在文件管理器中显示 {path}: {error}"))
}

/// Native "choose a folder" panel — the first-run wizard's data directory.
///
/// Returns an empty string when the user cancels, so the caller can tell
/// "cancelled" apart from "chose the empty path".
#[tauri::command]
async fn pick_folder<R: Runtime>(
    window: tauri::WebviewWindow<R>,
    options: Option<PickOptions>,
) -> Result<String, String> {
    let options = options.unwrap_or_default();
    let mut builder = window.dialog().file();
    if let Some(title) = options.title {
        builder = builder.set_title(title);
    }
    match builder.blocking_pick_folder() {
        Some(folder) => folder
            .into_path()
            .map(|path| path.to_string_lossy().into_owned())
            .map_err(|error| format!("无法解析所选目录: {error}")),
        None => Ok(String::new()),
    }
}

/// Read a local file into the webview so it can travel the normal upload path.
#[tauri::command]
fn read_local_file(path: String) -> Result<LocalFilePayload, String> {
    let path = PathBuf::from(&path);
    let metadata = std::fs::metadata(&path)
        .map_err(|error| format!("无法读取 {}: {error}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!("{} 不是文件", path.display()));
    }
    if metadata.len() > MAX_LOCAL_FILE_BYTES {
        return Err(format!(
            "文件过大（{} MB），请使用应用内的上传入口",
            metadata.len() / (1024 * 1024)
        ));
    }
    let bytes =
        std::fs::read(&path).map_err(|error| format!("无法读取 {}: {error}", path.display()))?;
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .ok_or_else(|| format!("{} 没有文件名", path.display()))?;
    Ok(LocalFilePayload {
        path: path.to_string_lossy().into_owned(),
        name,
        size: metadata.len(),
        mime: mime_for(&path),
        base64: base64::engine::general_purpose::STANDARD.encode(&bytes),
    })
}

/// Relaunch the shell in place (used after a data-directory change).
#[tauri::command]
fn restart_app<R: Runtime>(app: tauri::AppHandle<R>) {
    app.restart();
}

/// Best-effort MIME type from the extension. The webview builds a `File` from
/// the bytes, and the backend re-sniffs the content anyway.
fn mime_for(path: &std::path::Path) -> Option<String> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    let mime = match extension.as_str() {
        "pdf" => "application/pdf",
        "epub" => "application/epub+zip",
        "md" | "markdown" => "text/markdown",
        "txt" | "text" | "log" => "text/plain",
        "html" | "htm" => "text/html",
        "json" => "application/json",
        "csv" => "text/csv",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        _ => return None,
    };
    Some(mime.to_string())
}

/// Register the plugin. The caller passes the shell implementation.
pub fn init<R: Runtime>(backend: Arc<dyn DesktopBackend>) -> TauriPlugin<R> {
    // Turbofish on both parameters: `Builder` is `Builder<R, C = ()>`, and
    // leaving `C` to inference makes the chain resolve to the default runtime.
    Builder::<R, ()>::new("deeptutor")
        .invoke_handler(tauri::generate_handler![
            desktop_status,
            restart_service,
            shell_settings,
            update_shell_settings,
            note_ui_ready,
            notify_round_complete,
            take_notification_target,
            take_open_request,
            first_run_state,
            apply_first_run,
            check_updates,
            install_shell_update,
            log_event,
            pick_files,
            pick_folder,
            reveal_in_folder,
            read_local_file,
            restart_app
        ])
        .setup(move |app, _api| {
            // The trait object is cheap to clone and the closure runs once.
            app.manage(BackendState(Arc::clone(&backend)));
            Ok(())
        })
        .build()
}
