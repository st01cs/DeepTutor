//! Owns the Python launcher and hands the window over to it when ready.
//!
//! Deliberately thin: port selection, settings persistence, the frontend build
//! and the update handshake all stay in `deeptutor/runtime/launcher.py` so that
//! the desktop path cannot drift from CLI/Web behaviour. This module only does
//! what the shell alone can do — spawn, watch, navigate, and clean up.

use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tauri::WebviewWindow;

use crate::runtime_info::{RuntimeInfo, SUPPORTED_SCHEMA_VERSION};

/// The launcher can spend minutes on a first production frontend build; the
/// timeout only exists so a wedged child does not leave a splash forever.
const READY_TIMEOUT: Duration = Duration::from_secs(300);
const POLL_INTERVAL: Duration = Duration::from_millis(250);
const TERM_GRACE: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub struct ShellConfig {
    /// Runtime home: owns `data/`, `desktop/` and (Phase 2) `runtimes/`.
    pub home: PathBuf,
    /// Working directory for the launcher: the checkout during Phase 0, the
    /// runtime pack afterwards.
    pub workdir: PathBuf,
    pub python: PathBuf,
    pub state_path: PathBuf,
    pub log_path: PathBuf,
}

impl ShellConfig {
    pub fn resolve() -> Self {
        let home = resolve_home();
        let desktop_dir = home.join("desktop");
        let workdir = env_path("DEEPTUTOR_DESKTOP_WORKDIR").unwrap_or_else(|| home.clone());
        let python = env_path("DEEPTUTOR_DESKTOP_PYTHON").unwrap_or_else(|| default_python(&home));
        Self {
            state_path: desktop_dir.join("runtime.json"),
            log_path: desktop_dir.join("logs").join("launcher.log"),
            home,
            workdir,
            python,
        }
    }
}

fn env_path(key: &str) -> Option<PathBuf> {
    let raw = std::env::var(key).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(PathBuf::from(trimmed))
}

/// `DEEPTUTOR_HOME` wins so a developer can point the shell at a checkout.
fn resolve_home() -> PathBuf {
    if let Some(explicit) = env_path("DEEPTUTOR_HOME") {
        return explicit;
    }
    if cfg!(windows) {
        if let Some(appdata) = env_path("LOCALAPPDATA") {
            return appdata.join("DeepTutor");
        }
    }
    if let Some(home) = env_path("HOME") {
        if cfg!(target_os = "macos") {
            return home
                .join("Library")
                .join("Application Support")
                .join("DeepTutor");
        }
        return home.join(".local").join("share").join("DeepTutor");
    }
    PathBuf::from("DeepTutor")
}

fn default_python(home: &Path) -> PathBuf {
    let venv = home.join(".venv");
    let candidate = if cfg!(windows) {
        venv.join("Scripts").join("python.exe")
    } else {
        venv.join("bin").join("python")
    };
    if candidate.exists() {
        return candidate;
    }
    PathBuf::from(if cfg!(windows) { "python" } else { "python3" })
}

pub struct Supervisor {
    config: ShellConfig,
    window: Mutex<Option<WebviewWindow>>,
    child: Mutex<Option<Child>>,
    last_status: Mutex<String>,
    stopping: AtomicBool,
}

impl Supervisor {
    pub fn new(config: ShellConfig) -> Self {
        Self {
            config,
            window: Mutex::new(None),
            child: Mutex::new(None),
            last_status: Mutex::new("starting".to_string()),
            stopping: AtomicBool::new(false),
        }
    }

    pub fn attach_window(&self, window: WebviewWindow) {
        *self.window.lock().expect("window lock poisoned") = Some(window);
    }

    pub fn start(self: &Arc<Self>) {
        let this = Arc::clone(self);
        std::thread::Builder::new()
            .name("deeptutor-supervisor".to_string())
            .spawn(move || this.run())
            .expect("failed to spawn the supervisor thread");
    }

    fn run(&self) {
        self.set_status("正在启动本地服务 ...");
        if let Err(error) = self.spawn_launcher() {
            self.fail(&error);
            return;
        }

        let deadline = Instant::now() + READY_TIMEOUT;
        loop {
            if self.stopping.load(Ordering::SeqCst) {
                return;
            }
            if let Some(info) = RuntimeInfo::read(&self.config.state_path) {
                // Only trust state written by *our* launcher. A leftover file
                // from a previous run reads as an instant "stopped" failure
                // (or worse, navigates to a dead port), which is exactly the
                // bug this guard was added for.
                let expected_pid = self.launcher_pid().map(|pid| pid as i32);
                if expected_pid.is_some() && info.pid != expected_pid {
                    std::thread::sleep(POLL_INTERVAL);
                    continue;
                }
                if info.schema_version != SUPPORTED_SCHEMA_VERSION {
                    self.fail(&format!(
                        "运行时状态文件版本不受支持 ({})，请更新桌面应用。",
                        info.schema_version
                    ));
                    return;
                }
                match info.status.as_str() {
                    "ready" => {
                        match info.frontend_url.clone() {
                            Some(url) => self.open(&url),
                            None => self.fail("运行时状态缺少 frontend_url，无法打开界面。"),
                        }
                        return;
                    }
                    "stopped" => {
                        self.fail("本地服务在就绪前退出，请查看日志。");
                        return;
                    }
                    _ => {}
                }
            }
            if let Some(code) = self.take_exit_code() {
                self.fail(&format!("本地服务启动失败（退出码 {code}），请查看日志。"));
                return;
            }
            if Instant::now() >= deadline {
                self.fail("本地服务启动超时，请查看日志。");
                return;
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    fn spawn_launcher(&self) -> Result<(), String> {
        if let Some(parent) = self.config.log_path.parent() {
            fs::create_dir_all(parent).map_err(|error| format!("无法创建日志目录: {error}"))?;
        }
        // Start from a clean slate: the launcher writes "starting" only after
        // its own imports, and the poll loop must not read last run's file in
        // that window.
        let _ = fs::remove_file(&self.config.state_path);
        let stdout = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.config.log_path)
            .map_err(|error| format!("无法打开日志文件: {error}"))?;
        let stderr = stdout
            .try_clone()
            .map_err(|error| format!("无法复用日志句柄: {error}"))?;

        let mut command = Command::new(&self.config.python);
        command
            .arg("-m")
            .arg("deeptutor_cli.main")
            .arg("start")
            .arg("--home")
            .arg(&self.config.home)
            .arg("--no-browser")
            .arg("--auto-ports")
            .arg("--runtime-info")
            .arg(&self.config.state_path)
            .arg("--parent-pid")
            .arg(std::process::id().to_string())
            .current_dir(&self.config.workdir)
            .env("DEEPTUTOR_DESKTOP_SHELL", "1")
            .env("DEEPTUTOR_HOME", &self.config.home)
            .env("DEEPTUTOR_LAUNCHER_PID", std::process::id().to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr));

        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // Own process group: SIGTERM to the group reaches uvicorn and Node,
            // including children the launcher spawned for itself.
            command.process_group(0);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }

        let child = command.spawn().map_err(|error| {
            format!(
                "无法启动 Python launcher ({})：{error}",
                self.config.python.display()
            )
        })?;
        *self.child.lock().expect("child lock poisoned") = Some(child);
        Ok(())
    }

    fn take_exit_code(&self) -> Option<i32> {
        let mut guard = self.child.lock().expect("child lock poisoned");
        let child = guard.as_mut()?;
        match child.try_wait() {
            Ok(Some(status)) => Some(status.code().unwrap_or(-1)),
            Ok(None) => None,
            Err(_) => Some(-1),
        }
    }

    fn launcher_pid(&self) -> Option<u32> {
        let guard = self.child.lock().ok()?;
        guard.as_ref().map(|child| child.id())
    }

    fn open(&self, url: &str) {
        self.set_status(&format!("已就绪：{url}"));
        let parsed = match tauri::Url::parse(url) {
            Ok(parsed) => parsed,
            Err(error) => {
                self.fail(&format!("无法解析前端地址 {url}：{error}"));
                return;
            }
        };
        let result = {
            let guard = self.window.lock().expect("window lock poisoned");
            match guard.as_ref() {
                Some(window) => window.navigate(parsed).map_err(|error| error.to_string()),
                None => Err("窗口尚未创建".to_string()),
            }
        };
        if let Err(error) = result {
            self.fail(&format!("无法打开前端页面：{error}"));
            return;
        }
        self.verify_remote_ipc();
    }

    /// Probe IPC from *inside* the loopback page.
    ///
    /// The splash runs on the app origin, where IPC needs no `remote` grant, so
    /// a successful button press there proves nothing about the capability the
    /// real UI depends on. These evals run in the loopback document and report
    /// through the URL fragment, because the interesting failure is "the
    /// bridge is missing or rejected", which IPC itself cannot report.
    ///
    /// Verified 2026-09-24 (Phase 0): core/plugin commands reach the shell from
    /// the loopback origin and the `http://127.0.0.1:*` grant works, but
    /// **application commands do not** — `invoke("desktop_probe")` answers
    /// "not allowed. Plugin not found" because app commands carry no ACL entry.
    /// Shell features meant to be callable from the UI therefore have to ship as
    /// a Tauri plugin with a permission set, not via `generate_handler!` alone.
    fn verify_remote_ipc(&self) {
        let script = concat!(
            "(function () {",
            " var report = function (text) {",
            "  try { location.hash = 'deeptutorProbe=' + String(text).replace(/[^A-Za-z0-9_.:;|=\\-]/g, '_'); } catch (e) {}",
            " };",
            " if (!window.__TAURI__ || !window.__TAURI__.core) { report('no-bridge'); return; }",
            " var parts = [];",
            " var done = function () { if (parts.length === 2) { report(parts.join(' ; ')); } };",
            " var push = function (label, value) { parts.push(label + '=' + value); done(); };",
            " window.__TAURI__.core.invoke('plugin:app|version').then(",
            "  function (value) { push('core-app-ok', value); },",
            "  function (error) { push('core-app-error', error); });",
            " window.__TAURI__.core.invoke('desktop_probe').then(",
            "  function (value) { push('app-cmd-ok', JSON.stringify(value).slice(0, 80)); },",
            "  function (error) { push('app-cmd-error', error); });",
            "})();"
        );
        for attempt in 1..=3 {
            std::thread::sleep(Duration::from_secs(3));
            self.append_shell_log(&format!("remote-ipc probe dispatched (attempt {attempt})"));
            {
                let guard = self.window.lock().expect("window lock poisoned");
                let Some(window) = guard.as_ref() else {
                    return;
                };
                if window.eval(script).is_err() {
                    self.append_shell_log("remote-ipc probe eval failed");
                    return;
                }
            }
            let deadline = Instant::now() + Duration::from_secs(4);
            while Instant::now() < deadline {
                if let Some(report) = self.probe_report() {
                    self.append_shell_log(&format!("remote-ipc probe result: {report}"));
                    self.clear_probe_hash();
                    return;
                }
                std::thread::sleep(Duration::from_millis(250));
            }
        }
        self.append_shell_log("remote-ipc probe: no report from the loopback page");
    }

    /// Read the `#deeptutorProbe=...` report the evaled script leaves behind.
    fn probe_report(&self) -> Option<String> {
        let guard = self.window.lock().ok()?;
        let window = guard.as_ref()?;
        let fragment = window.url().ok()?.fragment().map(str::to_string)?;
        let marker = "deeptutorProbe=";
        let index = fragment.find(marker)?;
        Some(fragment[index + marker.len()..].to_string())
    }

    fn clear_probe_hash(&self) {
        let guard = self.window.lock().expect("window lock poisoned");
        if let Some(window) = guard.as_ref() {
            let _ = window.eval("try { history.replaceState(null, '', location.pathname + location.search); } catch (e) {}");
        }
    }

    /// Called by the `desktop_probe` command so the log records which origin
    /// actually reached the shell.
    pub fn record_probe(&self, origin: &str) {
        self.append_shell_log(&format!("webview-ipc ok: invoke received from {origin}"));
    }

    fn set_status(&self, text: &str) {
        *self.last_status.lock().expect("status lock poisoned") = text.to_string();
        let guard = self.window.lock().expect("window lock poisoned");
        if let Some(window) = guard.as_ref() {
            let script = format!("window.setStatus({});", json_string(text));
            let _ = window.eval(&script);
        }
    }

    fn fail(&self, message: &str) {
        self.append_shell_log(message);
        *self.last_status.lock().expect("status lock poisoned") = message.to_string();
        {
            let guard = self.window.lock().expect("window lock poisoned");
            if let Some(window) = guard.as_ref() {
                let _ = window.set_title("DeepTutor — 启动失败");
                let script = format!("window.showError({});", json_string(message));
                let _ = window.eval(&script);
            }
        }
        // A failed handshake must not leave a half-started backend/frontend
        // pair running behind an error splash.
        self.stop();
    }

    fn append_shell_log(&self, message: &str) {
        let Some(parent) = self.config.log_path.parent() else {
            return;
        };
        if fs::create_dir_all(parent).is_err() {
            return;
        }
        let shell_log = parent.join("shell.log");
        if let Ok(mut handle) = OpenOptions::new().create(true).append(true).open(shell_log) {
            use std::io::Write as _;
            let _ = writeln!(handle, "[shell] {message}");
        }
    }

    /// Stop the launcher and everything it spawned. Safe to call twice.
    pub fn stop(&self) {
        if self.stopping.swap(true, Ordering::SeqCst) {
            return;
        }
        let mut guard = self.child.lock().expect("child lock poisoned");
        let Some(child) = guard.as_mut() else {
            return;
        };
        signal_group(child, true);
        let deadline = Instant::now() + TERM_GRACE;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(100));
                }
                Ok(None) => {
                    signal_group(child, false);
                    let _ = child.kill();
                    let _ = child.wait();
                    return;
                }
                Err(_) => return,
            }
        }
    }

    /// Self-test payload for the remote-IPC probe in the splash window.
    pub fn probe(&self) -> serde_json::Value {
        let status = self
            .last_status
            .lock()
            .map(|value| value.clone())
            .unwrap_or_default();
        let runtime = RuntimeInfo::read(&self.config.state_path).map(|info| {
            serde_json::json!({
                "schema_version": info.schema_version,
                "status": info.status,
                "frontend_url": info.frontend_url,
                "backend_url": info.backend_url,
                "backend_port": info.backend_port,
                "frontend_port": info.frontend_port,
                "launcher_pid": info.pid,
                // The token itself stays out of the webview: it is the
                // shell<->launcher handshake secret.
                "token_present": info.token.is_some(),
            })
        });
        serde_json::json!({
            "shell": "deeptutor-desktop",
            "phase": "phase-0",
            "home": self.config.home.to_string_lossy(),
            "workdir": self.config.workdir.to_string_lossy(),
            "python": self.config.python.to_string_lossy(),
            "status": status,
            "runtime": runtime,
        })
    }
}

#[cfg(unix)]
fn signal_group(child: &Child, terminate: bool) {
    let signal = if terminate { libc::SIGTERM } else { libc::SIGKILL };
    // The launcher was spawned with process_group(0), so its pid is the pgid.
    unsafe {
        libc::kill(-(child.id() as i32), signal);
    }
}

#[cfg(windows)]
fn signal_group(child: &Child, _terminate: bool) {
    // taskkill /T is the only portable way to reach the launcher's own
    // children (uvicorn + Node); console processes get no CTRL_BREAK from
    // here, so both the graceful and the forced path use /F.
    let _ = Command::new("taskkill")
        .arg("/PID")
        .arg(child.id().to_string())
        .arg("/T")
        .arg("/F")
        .status();
}

fn json_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string())
}
