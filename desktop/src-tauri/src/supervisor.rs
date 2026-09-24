//! Owns the Python launcher and hands the window over to it when ready.
//!
//! Deliberately thin: port selection, settings persistence, the frontend build
//! and the update handshake all stay in `deeptutor/runtime/launcher.py` so the
//! desktop path cannot drift from CLI/Web behaviour. This module only does what
//! the shell alone can do — spawn, watch, restart, navigate, clean up.

use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use tauri::{AppHandle, WebviewWindow};
use tauri_plugin_deeptutor::{
    DesktopBackend, DesktopStatus, FirstRunChoices, FirstRunOutcome, FirstRunState,
    NotificationOutcome, NotificationRequest, NotificationTarget, OpenRequestPayload,
    RuntimeSnapshot, RuntimeUpdateReport, SettingsPatch, ShellSettingsSnapshot, WindowGeometry,
};
use tauri_plugin_dialog::{DialogExt, MessageDialogKind};
use tauri_plugin_notification::NotificationExt;

use crate::handoff::OpenQueue;
use crate::notify::NotificationCenter;
use crate::runtime_info::{RuntimeInfo, SUPPORTED_SCHEMA_VERSION};
use crate::runtime_pack::{InstalledPack, PackInstaller};
use crate::settings::{Bootstrap, ShellSettings};

/// The launcher can spend minutes on a first production frontend build; this
/// timeout only exists so a wedged child does not leave a splash forever.
const READY_TIMEOUT: Duration = Duration::from_secs(300);
const POLL_INTERVAL: Duration = Duration::from_millis(250);
const TERM_GRACE: Duration = Duration::from_secs(5);
/// Crash restarts before the shell stops retrying and reports the failure.
const MAX_CRASH_RESTARTS: u32 = 3;
const CRASH_BACKOFF_BASE: Duration = Duration::from_secs(2);

#[derive(Debug, Clone)]
pub struct ShellConfig {
    /// Runtime home: owns `data/`, `desktop/` and (Phase 2) `runtimes/`.
    pub home: PathBuf,
    /// The platform default home, i.e. where `bootstrap.json` is looked for.
    /// Different from `home` once the first-run wizard moves the data directory.
    pub default_home: PathBuf,
    /// Working directory for the launcher: the checkout in Phase 0/1, the
    /// runtime pack afterwards.
    pub workdir: PathBuf,
    /// Explicit interpreter from `DEEPTUTOR_DESKTOP_PYTHON`, when set.
    pub python_override: Option<PathBuf>,
    pub state_path: PathBuf,
    pub logs_dir: PathBuf,
}

impl ShellConfig {
    pub fn resolve() -> Self {
        Self::resolve_full(&|key| std::env::var(key).ok(), &Bootstrap::read_home)
    }

    /// Same as [`ShellConfig::resolve`] with an injectable environment read, so
    /// the resolution rules can be tested without touching the real machine or a
    /// real bootstrap file.
    #[cfg(test)]
    pub fn resolve_with(read_env: &dyn Fn(&str) -> Option<String>) -> Self {
        Self::resolve_full(read_env, &|_| None)
    }

    /// Full form: environment *and* the bootstrap pointer are injectable.
    pub fn resolve_full(
        read_env: &dyn Fn(&str) -> Option<String>,
        read_bootstrap: &dyn Fn(&Path) -> Option<PathBuf>,
    ) -> Self {
        let env = |key: &str| env_path_from(read_env(key));
        let default_home = resolve_home_with(&env);
        // A bootstrap pointer only ever *redirects* the default home; the
        // pointer itself is always read from the default location, so a broken
        // pointer cannot compound into a second profile.
        let home = read_bootstrap(&default_home).unwrap_or_else(|| default_home.clone());
        let workdir = env("DEEPTUTOR_DESKTOP_WORKDIR").unwrap_or_else(|| home.clone());
        Self {
            state_path: home.join("desktop").join("runtime.json"),
            logs_dir: home.join("desktop").join("logs"),
            python_override: env("DEEPTUTOR_DESKTOP_PYTHON"),
            default_home,
            home,
            workdir,
        }
    }

    /// Interpreters to try, most specific first.
    ///
    /// The workdir venv matters: someone who simply runs the shell from a
    /// checkout has a `.venv` next to the source, not inside the app-data home.
    /// Phase 0 shipped without that candidate, so the shell fell through to the
    /// system Python and reported a bare "exit code 1".
    pub fn interpreter_candidates(&self) -> Vec<InterpreterCandidate> {
        let mut candidates = Vec::new();
        if let Some(explicit) = self.python_override.clone() {
            candidates.push(InterpreterCandidate {
                path: explicit,
                source: "DEEPTUTOR_DESKTOP_PYTHON",
                must_exist: true,
            });
        }
        // A runtime pack is the installed-product path: it needs no environment
        // variables at all, which is the whole point of Phase 2.
        if let Some(pack) = self.active_pack() {
            candidates.push(InterpreterCandidate {
                path: pack.manifest.python_path(&pack.dir),
                source: "runtime pack",
                must_exist: true,
            });
        }
        candidates.push(InterpreterCandidate {
            path: venv_python(&self.home),
            source: "<home>/.venv",
            must_exist: true,
        });
        if self.workdir != self.home {
            candidates.push(InterpreterCandidate {
                path: venv_python(&self.workdir),
                source: "<workdir>/.venv",
                must_exist: true,
            });
        }
        for name in path_python_names() {
            candidates.push(InterpreterCandidate {
                path: PathBuf::from(name),
                source: "PATH",
                must_exist: false,
            });
        }
        candidates
    }

    /// The runtime pack the shell was told to use, if any.
    pub fn active_pack(&self) -> Option<InstalledPack> {
        PackInstaller::new(&self.home).active()
    }

    /// First candidate that can actually import the launcher.
    ///
    /// Probing costs a few hundred milliseconds and buys a failure message a
    /// user can act on, instead of a launcher that exits 1 for reasons the
    /// dialog never mentions.
    pub fn resolve_interpreter(&self) -> Result<InterpreterCandidate, String> {
        let mut tried: Vec<String> = Vec::new();
        for candidate in self.interpreter_candidates() {
            if candidate.must_exist && !candidate.path.exists() {
                tried.push(format!("{} — 不存在", candidate.path.display()));
                continue;
            }
            if probe_interpreter(&candidate.path) {
                return Ok(candidate);
            }
            tried.push(format!(
                "{} — 无法导入 deeptutor_cli",
                candidate.path.display()
            ));
        }
        Err(format!(
            "找不到可用的 DeepTutor 运行环境。已尝试：\n{}\n\n\
             修复方式（任选其一）：\n\
             1) 在仓库根目录创建虚拟环境：uv venv && uv pip install -e \".[cli,server]\"\n\
             2) 设置 DEEPTUTOR_DESKTOP_PYTHON 指向可用的解释器\n\
             3) 运行 `deeptutor-desktop --self-check` 查看解析结果",
            tried.join("\n")
        ))
    }
}

/// One interpreter candidate plus where it came from, for diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterpreterCandidate {
    pub path: PathBuf,
    pub source: &'static str,
    /// PATH lookups cannot be pre-checked; venv paths can.
    pub must_exist: bool,
}

fn env_path_from(raw: Option<String>) -> Option<PathBuf> {
    let raw = raw?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(PathBuf::from(trimmed))
}

/// `DEEPTUTOR_HOME` wins so a developer can point the shell at a checkout.
fn resolve_home_with(env: &dyn Fn(&str) -> Option<PathBuf>) -> PathBuf {
    if let Some(explicit) = env("DEEPTUTOR_HOME") {
        return explicit;
    }
    if cfg!(windows) {
        if let Some(appdata) = env("LOCALAPPDATA") {
            return appdata.join("DeepTutor");
        }
    }
    if let Some(home) = env("HOME") {
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

fn venv_python(root: &Path) -> PathBuf {
    let venv = root.join(".venv");
    if cfg!(windows) {
        venv.join("Scripts").join("python.exe")
    } else {
        venv.join("bin").join("python")
    }
}

fn path_python_names() -> &'static [&'static str] {
    if cfg!(windows) {
        &["python.exe", "python"]
    } else {
        &["python3", "python"]
    }
}

/// Does this interpreter have DeepTutor's CLI installed?
fn probe_interpreter(path: &Path) -> bool {
    let mut command = Command::new(path);
    command
        .arg("-c")
        .arg("import deeptutor_cli.main")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// Outcome of one launch attempt.
enum Handshake {
    /// A quit (or a newer generation) interrupted the wait.
    Shutdown,
    /// Fatal: the launcher could not start or never became ready.
    Failed(String),
    Ready,
}

/// How a launcher that had already reported ready stopped.
enum Monitor {
    Shutdown,
    Exited(i32),
}

pub struct Supervisor {
    config: ShellConfig,
    window: Mutex<Option<WebviewWindow>>,
    child: Mutex<Option<Child>>,
    last_status: Mutex<String>,
    /// Phase 3: user preferences, the notification bookkeeping and pending OS
    /// hand-offs. All three outlive a launcher restart by design.
    settings: Mutex<ShellSettings>,
    notifications: NotificationCenter,
    handoffs: OpenQueue,
    /// Set when a first-run choice (the data directory) only takes effect on
    /// the next launch.
    restart_required: AtomicBool,
    stopping: AtomicBool,
    /// Bumped on every start so a superseded supervisor thread exits instead of
    /// polling (and navigating) alongside its replacement.
    generation: AtomicU64,
    launch_count: AtomicU64,
    restarts: AtomicU64,
    /// How many times the remote-IPC self test has run; only the first round
    /// exercises `restart_service` (otherwise every navigation would restart
    /// the service again, forever).
    probe_rounds: AtomicU64,
    restart_lock: Mutex<()>,
    /// Set once by `new_shared`; lets a `&self` method (the IPC restart path)
    /// spawn the next supervisor thread.
    self_ref: OnceLock<Weak<Supervisor>>,
    /// Set once by `attach_app`; used for the native failure dialog.
    app: OnceLock<AppHandle>,
    /// The interpreter the launcher was actually started with, for diagnostics.
    interpreter: Mutex<Option<InterpreterCandidate>>,
}

impl Supervisor {
    pub fn new_shared(config: ShellConfig) -> Arc<Self> {
        let settings = ShellSettings::load(&config.home);
        let notifications = NotificationCenter::new(settings.notifications);
        let supervisor = Arc::new(Self {
            config,
            window: Mutex::new(None),
            child: Mutex::new(None),
            last_status: Mutex::new("starting".to_string()),
            settings: Mutex::new(settings),
            notifications,
            handoffs: OpenQueue::new(),
            restart_required: AtomicBool::new(false),
            stopping: AtomicBool::new(false),
            generation: AtomicU64::new(0),
            launch_count: AtomicU64::new(0),
            restarts: AtomicU64::new(0),
            probe_rounds: AtomicU64::new(0),
            restart_lock: Mutex::new(()),
            self_ref: OnceLock::new(),
            app: OnceLock::new(),
            interpreter: Mutex::new(None),
        });
        let _ = supervisor.self_ref.set(Arc::downgrade(&supervisor));
        supervisor
    }

    pub fn attach_window(&self, window: WebviewWindow) {
        *self.window.lock().expect("window lock poisoned") = Some(window);
    }

    pub fn attach_app(&self, app: AppHandle) {
        let _ = self.app.set(app);
    }

    pub fn start(&self) {
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        self.spawn_thread(generation);
    }

    fn spawn_thread(&self, generation: u64) {
        let Some(this) = self.self_ref.get().and_then(Weak::upgrade) else {
            return;
        };
        std::thread::Builder::new()
            .name("deeptutor-supervisor".to_string())
            .spawn(move || this.run(generation))
            .expect("failed to spawn the supervisor thread");
    }

    fn is_current(&self, generation: u64) -> bool {
        self.generation.load(Ordering::SeqCst) == generation
            && !self.stopping.load(Ordering::SeqCst)
    }

    fn run(&self, generation: u64) {
        let mut crashes = 0u32;
        // The first-run wizard runs in the splash (app origin). Nothing is
        // spawned until it has been answered: the launcher's `--home` depends on
        // the answer, and a half-configured first profile is worse than a
        // second of patience.
        if !self.await_first_run(generation) {
            return;
        }
        loop {
            if !self.is_current(generation) {
                return;
            }
            match self.handshake(generation) {
                Handshake::Shutdown => return,
                Handshake::Failed(message) => {
                    self.fail(&message);
                    return;
                }
                Handshake::Ready => {}
            }
            match self.monitor(generation) {
                Monitor::Shutdown => return,
                Monitor::Exited(code) => {
                    crashes += 1;
                    if crashes > MAX_CRASH_RESTARTS {
                        self.fail(&format!(
                            "本地服务连续 {MAX_CRASH_RESTARTS} 次异常退出（最后退出码 {code}），已停止自动重启。"
                        ));
                        return;
                    }
                    self.restarts.fetch_add(1, Ordering::SeqCst);
                    let backoff = CRASH_BACKOFF_BASE * (1u32 << (crashes - 1));
                    self.set_status(&format!(
                        "本地服务异常退出（退出码 {code}），{} 秒后自动重启（第 {crashes}/{MAX_CRASH_RESTARTS} 次）...",
                        backoff.as_secs()
                    ));
                    if !self.sleep_interruptible(backoff, generation) {
                        return;
                    }
                    self.set_status("正在重启本地服务 ...");
                }
            }
        }
    }

    /// Block the launch until the wizard has been answered (or the app quits).
    fn await_first_run(&self, generation: u64) -> bool {
        if !self.needs_first_run() {
            return true;
        }
        self.set_status("首次启动：请在下方完成设置 ...");
        // Without this line, "the app opened but nothing ever started" has no
        // trace at all: the gate deliberately spawns no process to inspect.
        self.append_shell_log("waiting for the first-run wizard to be completed");
        let started = Instant::now();
        let mut announced = false;
        loop {
            if !self.is_current(generation) {
                return false;
            }
            if !self.needs_first_run() {
                return true;
            }
            // A splash that never loads must not look like a hang; say what we
            // are waiting for, once, and keep waiting (the tray can still quit).
            if !announced && started.elapsed() > Duration::from_secs(120) {
                announced = true;
                self.append_shell_log("still waiting for the first-run wizard to be completed");
                self.set_status("仍在等待完成首次设置（可在托盘菜单退出）...");
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    fn needs_first_run(&self) -> bool {
        self.settings
            .lock()
            .map(|settings| settings.needs_first_run())
            .unwrap_or(false)
    }

    /// Spawn the launcher and wait for its ready handshake.
    fn handshake(&self, generation: u64) -> Handshake {
        self.set_status("正在启动本地服务 ...");
        if let Err(error) = self.spawn_launcher() {
            return Handshake::Failed(error);
        }

        let deadline = Instant::now() + READY_TIMEOUT;
        loop {
            if !self.is_current(generation) {
                return Handshake::Shutdown;
            }
            if let Some(info) = RuntimeInfo::read(&self.config.state_path) {
                // Only trust state written by *our* launcher. A leftover file
                // from a previous run reads as an instant "stopped" failure (or
                // worse, navigates to a dead port) — the Phase 0 bug this guard
                // exists for.
                let expected_pid = self.launcher_pid().map(|pid| pid as i32);
                if expected_pid.is_some() && info.pid != expected_pid {
                    std::thread::sleep(POLL_INTERVAL);
                    continue;
                }
                if info.schema_version != SUPPORTED_SCHEMA_VERSION {
                    return Handshake::Failed(format!(
                        "运行时状态文件版本不受支持 ({})，请更新桌面应用。",
                        info.schema_version
                    ));
                }
                match info.status.as_str() {
                    "ready" => {
                        return match info.frontend_url.clone() {
                            Some(url) => {
                                self.open(&url);
                                Handshake::Ready
                            }
                            None => Handshake::Failed(
                                "运行时状态缺少 frontend_url，无法打开界面。".into(),
                            ),
                        };
                    }
                    "stopped" => {
                        return Handshake::Failed("本地服务在就绪前退出，请查看日志。".into())
                    }
                    _ => {}
                }
            }
            if let Some(code) = self.take_exit_code() {
                // "Exit code 1" alone sends people to the wrong place; the last
                // lines of the launcher log are what actually name the cause.
                let mut message = format!("本地服务启动失败（退出码 {code}）。");
                let tail = self.launcher_log_tail(6);
                if !tail.is_empty() {
                    message.push_str("\n\n");
                    message.push_str(&tail);
                }
                return Handshake::Failed(message);
            }
            if Instant::now() >= deadline {
                return Handshake::Failed("本地服务启动超时，请查看日志。".to_string());
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    /// Keep watching a launcher that already reported ready.
    fn monitor(&self, generation: u64) -> Monitor {
        loop {
            if !self.is_current(generation) {
                return Monitor::Shutdown;
            }
            if let Some(code) = self.take_exit_code() {
                self.append_shell_log(&format!("launcher exited with code {code}"));
                return Monitor::Exited(code);
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    /// Sleep in small slices so a quit or a manual restart still wins promptly.
    fn sleep_interruptible(&self, total: Duration, generation: u64) -> bool {
        let deadline = Instant::now() + total;
        while Instant::now() < deadline {
            if !self.is_current(generation) {
                return false;
            }
            std::thread::sleep(POLL_INTERVAL);
        }
        true
    }

    fn spawn_launcher(&self) -> Result<(), String> {
        if let Err(error) = fs::create_dir_all(&self.config.logs_dir) {
            return Err(format!("无法创建日志目录: {error}"));
        }
        let interpreter = self.config.resolve_interpreter()?;
        self.append_shell_log(&format!(
            "using interpreter {} (from {})",
            interpreter.path.display(),
            interpreter.source
        ));
        if let Ok(mut cache) = self.interpreter.lock() {
            *cache = Some(interpreter.clone());
        }
        // Start from a clean slate: the launcher writes "starting" only after
        // its own imports, and the poll loop must not read last run's file in
        // that window.
        let _ = fs::remove_file(&self.config.state_path);
        let stdout = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.config.logs_dir.join("launcher.log"))
            .map_err(|error| format!("无法打开日志文件: {error}"))?;
        let stderr = stdout
            .try_clone()
            .map_err(|error| format!("无法复用日志句柄: {error}"))?;

        let mut command = Command::new(&interpreter.path);
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

        // A pack brings its own Node; the launcher finds it through PATH
        // (`shutil.which("node")`) and would otherwise need the user to have one.
        if let Some(pack) = self.config.active_pack() {
            let node_dir = pack.manifest.node_dir(&pack.dir);
            if node_dir.exists() {
                let mut paths = vec![node_dir.clone()];
                if let Some(current) = std::env::var_os("PATH") {
                    paths.extend(std::env::split_paths(&current));
                }
                if let Ok(joined) = std::env::join_paths(paths) {
                    command.env("PATH", joined);
                }
                self.append_shell_log(&format!(
                    "runtime pack {} active; node from {}",
                    pack.pack_id,
                    node_dir.display()
                ));
            }
        }

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
                interpreter.path.display()
            )
        })?;
        *self.child.lock().expect("child lock poisoned") = Some(child);
        self.launch_count.fetch_add(1, Ordering::SeqCst);
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

    fn child_running(&self) -> bool {
        let mut guard = match self.child.lock() {
            Ok(guard) => guard,
            Err(_) => return false,
        };
        match guard.as_mut() {
            Some(child) => matches!(child.try_wait(), Ok(None)),
            None => false,
        }
    }

    fn open(&self, url: &str) {
        self.set_status(&format!("已就绪：{url}"));
        let parsed = match tauri::Url::parse(url) {
            Ok(parsed) => parsed,
            Err(error) => {
                self.append_shell_log(&format!("invalid frontend url {url}: {error}"));
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
            self.append_shell_log(&format!("navigate failed: {error}"));
            return;
        }
        self.verify_remote_ipc();
    }

    /// Probe IPC from *inside* the loopback page.
    ///
    /// The splash runs on the app origin, where IPC needs no `remote` grant, so
    /// a successful call there proves nothing about the capability the real UI
    /// depends on. These evals run in the loopback document and report through
    /// the URL fragment, because the interesting failure is "the bridge is
    /// missing or rejected", which IPC itself cannot report.
    ///
    /// Verified 2026-09-24 (Phase 0): core/plugin commands reach the shell from
    /// the loopback origin and the `http://127.0.0.1:*` grant works, but
    /// application commands do not — hence `plugin:deeptutor|desktop_status`.
    fn verify_remote_ipc(&self) {
        let round = self.probe_rounds.fetch_add(1, Ordering::SeqCst) + 1;
        let allow_restart = if round == 1 { "true" } else { "false" };
        // The flag has to travel with the probe: a separate eval lands on the
        // outgoing document while the navigation is still in flight.
        let script = format!("window.__deeptutorAllowRestart = {allow_restart};{PROBE_SCRIPT}");
        for attempt in 1..=3 {
            std::thread::sleep(Duration::from_secs(3));
            self.append_shell_log(&format!("remote-ipc probe dispatched (attempt {attempt})"));
            {
                let guard = self.window.lock().expect("window lock poisoned");
                let Some(window) = guard.as_ref() else {
                    return;
                };
                if window.eval(&script).is_err() {
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
                std::thread::sleep(POLL_INTERVAL);
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
        self.with_window(|window| {
            let _ = window.eval(
                "try { history.replaceState(null, '', location.pathname + location.search); } catch (e) {}",
            );
        });
    }

    pub fn set_status(&self, text: &str) {
        if let Ok(mut status) = self.last_status.lock() {
            *status = text.to_string();
        }
        let script = format!("window.setStatus({});", json_string(text));
        self.with_window(|window| {
            let _ = window.eval(&script);
        });
    }

    /// Surface a fatal shell error and make sure nothing keeps running behind it.
    pub fn fail(&self, message: &str) {
        self.append_shell_log(message);
        if let Ok(mut status) = self.last_status.lock() {
            *status = message.to_string();
        }
        let script = format!("window.showError({});", json_string(message));
        self.with_window(|window| {
            let _ = window.set_title("DeepTutor — 启动失败");
            let _ = window.eval(&script);
        });
        // A failed handshake must not leave a half-started backend/frontend
        // pair running behind an error splash.
        self.terminate_child();
        // A splash the user may not be looking at is not enough. This runs on
        // the supervisor thread, so blocking_show cannot deadlock the UI.
        if let Some(app) = self.app.get() {
            let _ = app
                .dialog()
                .message(message.to_string())
                .title("DeepTutor 启动失败")
                .kind(MessageDialogKind::Error)
                .blocking_show();
        }
    }

    fn with_window<F: FnOnce(&WebviewWindow)>(&self, action: F) {
        let guard = match self.window.lock() {
            Ok(guard) => guard,
            Err(_) => return,
        };
        if let Some(window) = guard.as_ref() {
            action(window);
        }
    }

    /// Append to `desktop/logs/shell.log`; never fails the caller.
    pub fn append_shell_log(&self, message: &str) {
        if fs::create_dir_all(&self.config.logs_dir).is_err() {
            return;
        }
        let path = self.config.logs_dir.join("shell.log");
        if let Ok(mut handle) = OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(handle, "[shell] {message}");
        }
    }

    pub fn logs_dir(&self) -> &Path {
        &self.config.logs_dir
    }

    /// Last non-empty lines of `launcher.log`, for the failure dialog.
    fn launcher_log_tail(&self, lines: usize) -> String {
        let Ok(text) = fs::read_to_string(self.config.logs_dir.join("launcher.log")) else {
            return String::new();
        };
        let mut tail: Vec<&str> = text
            .lines()
            .filter(|line| !line.trim().is_empty())
            .rev()
            .take(lines)
            .collect();
        tail.reverse();
        tail.join("\n")
    }

    /// SIGTERM the launcher's process group, then SIGKILL after a grace period.
    /// Safe to call when nothing is running.
    fn terminate_child(&self) {
        let mut guard = match self.child.lock() {
            Ok(guard) => guard,
            Err(_) => return,
        };
        let Some(child) = guard.as_mut() else {
            return;
        };
        signal_group(child, true);
        let deadline = Instant::now() + TERM_GRACE;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(100))
                }
                Ok(None) => {
                    signal_group(child, false);
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
                Err(_) => break,
            }
        }
        *guard = None;
    }

    /// Quit path: stop watching and take the launcher down. Idempotent.
    pub fn stop(&self) {
        if self.stopping.swap(true, Ordering::SeqCst) {
            return;
        }
        self.terminate_child();
    }

    /// Manual restart from the UI or the menu.
    pub fn restart(&self) -> Result<(), String> {
        let _guard = self
            .restart_lock
            .lock()
            .map_err(|_| "重启锁不可用".to_string())?;
        if self.stopping.load(Ordering::SeqCst) {
            return Err("应用正在退出，无法重启".to_string());
        }
        // A user-initiated restart also clears a previous "stopped" flag, so a
        // restart after a failed handshake genuinely retries.
        //
        // Retire the current supervisor thread *first*: otherwise it sees the
        // launcher we are about to kill as a crash and auto-restarts as well.
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        self.stopping.store(false, Ordering::SeqCst);
        self.terminate_child();
        let _ = fs::remove_file(&self.config.state_path);
        self.set_status("正在重新启动本地服务 ...");
        self.append_shell_log("manual restart requested");
        self.spawn_thread(generation);
        Ok(())
    }
}

impl DesktopBackend for Supervisor {
    fn status(&self) -> DesktopStatus {
        let runtime = RuntimeInfo::read(&self.config.state_path).map(|info| RuntimeSnapshot {
            schema_version: info.schema_version,
            status: info.status,
            frontend_url: info.frontend_url,
            backend_url: info.backend_url,
            backend_port: info.backend_port,
            frontend_port: info.frontend_port,
            token_present: info.token.is_some(),
        });
        DesktopStatus {
            launch_count: self.launch_count.load(Ordering::SeqCst),
            restarts: self.restarts.load(Ordering::SeqCst),
            launcher_running: self.child_running(),
            launcher_pid: self.launcher_pid(),
            message: self
                .last_status
                .lock()
                .map(|value| value.clone())
                .unwrap_or_default(),
            home: self.config.home.to_string_lossy().into_owned(),
            workdir: self.config.workdir.to_string_lossy().into_owned(),
            python: self.interpreter_for_display(),
            pack: self.config.active_pack().map(|pack| pack.pack_id),
            logs_dir: self.config.logs_dir.to_string_lossy().into_owned(),
            notifications_posted: self.notifications.posted(),
            window: self.window_geometry(),
            runtime,
        }
    }

    fn restart(&self) -> Result<(), String> {
        Supervisor::restart(self)
    }

    fn note_caller(&self, origin: &str) {
        self.append_shell_log(&format!("webview-ipc ok: invoke received from {origin}"));
    }

    fn settings(&self) -> ShellSettingsSnapshot {
        Supervisor::shell_settings(self)
    }

    fn update_settings(&self, patch: SettingsPatch) -> Result<ShellSettingsSnapshot, String> {
        Supervisor::apply_settings_patch(self, patch)
    }

    fn notify(&self, request: NotificationRequest) -> Result<NotificationOutcome, String> {
        Supervisor::notify_round(self, request)
    }

    fn take_notification_target(&self) -> Option<NotificationTarget> {
        self.notifications.take()
    }

    fn take_open_request(&self) -> Option<OpenRequestPayload> {
        self.handoffs.take()
    }

    fn first_run(&self) -> FirstRunState {
        Supervisor::first_run_state(self)
    }

    fn apply_first_run(&self, choices: FirstRunChoices) -> Result<FirstRunOutcome, String> {
        Supervisor::apply_first_run(self, choices)
    }

    fn check_runtime_updates(&self) -> RuntimeUpdateReport {
        Supervisor::check_runtime_updates(self)
    }
}

/// Phase 3 shell behaviour: preferences, notifications, OS hand-offs and the
/// "check updates" report. Kept in its own block so the launch/monitor state
/// machine above stays readable.
impl Supervisor {
    pub fn shell_settings(&self) -> ShellSettingsSnapshot {
        let settings = self
            .settings
            .lock()
            .map(|settings| settings.clone())
            .unwrap_or_default();
        ShellSettingsSnapshot {
            close_to_tray: settings.close_to_tray,
            notifications: settings.notifications,
            locale: settings.locale,
            first_run_completed: settings.first_run_completed,
            pack_catalog: settings.pack_catalog,
            home: self.config.home.to_string_lossy().into_owned(),
            default_home: self.config.default_home.to_string_lossy().into_owned(),
            restart_required: self.restart_required.load(Ordering::SeqCst),
        }
    }

    /// Apply a partial preference update and persist it.
    pub fn apply_settings_patch(
        &self,
        patch: SettingsPatch,
    ) -> Result<ShellSettingsSnapshot, String> {
        let mut guard = self
            .settings
            .lock()
            .map_err(|_| "设置不可用（锁已损坏）".to_string())?;
        if let Some(value) = patch.close_to_tray {
            guard.close_to_tray = value;
        }
        if let Some(value) = patch.notifications {
            guard.notifications = value;
        }
        if let Some(value) = patch.locale {
            let value = value.trim().to_string();
            if !value.is_empty() && value != "zh-CN" && value != "en" {
                return Err(format!("不支持的语言：{value}"));
            }
            guard.locale = value;
        }
        if let Some(value) = patch.first_run_completed {
            guard.first_run_completed = value;
        }
        if let Some(value) = patch.pack_catalog {
            let value = value.trim().to_string();
            guard.pack_catalog = if value.is_empty() { None } else { Some(value) };
        }
        guard.save(&self.config.home)?;
        // The notification preference is read on the hot path, so mirror it
        // instead of taking the settings lock for every round that finishes.
        self.notifications.set_enabled(guard.notifications);
        drop(guard);
        Ok(self.shell_settings())
    }

    /// Post the "round finished" notification and remember where it points.
    pub fn notify_round(
        &self,
        request: NotificationRequest,
    ) -> Result<NotificationOutcome, String> {
        if !self.notifications.is_enabled() {
            // A preference flipped off must not leave an old "come back to this
            // session" pointer armed for the next focus change.
            self.notifications.clear();
            return Ok(NotificationOutcome {
                delivered: false,
                permission: "disabled".to_string(),
                detail: Some("桌面通知已在设置中关闭".to_string()),
            });
        }
        let app = self
            .app
            .get()
            .cloned()
            .ok_or_else(|| "应用句柄尚未就绪".to_string())?;
        match app
            .notification()
            .builder()
            .title(request.title.clone())
            .body(request.body.clone())
            .show()
        {
            Ok(()) => {
                self.append_shell_log(&format!(
                    "notification posted: kind={} route={}",
                    request.kind.as_deref().unwrap_or("unknown"),
                    request.route
                ));
                self.notifications.record(&request);
                Ok(NotificationOutcome {
                    delivered: true,
                    permission: "granted".to_string(),
                    detail: None,
                })
            }
            Err(error) => {
                self.append_shell_log(&format!("notification failed: {error}"));
                Err(format!("无法发送系统通知：{error}"))
            }
        }
    }

    pub fn first_run_state(&self) -> FirstRunState {
        let snapshot = self.shell_settings();
        FirstRunState {
            completed: snapshot.first_run_completed,
            locale: snapshot.locale,
            default_locale: default_locale(),
            close_to_tray: snapshot.close_to_tray,
            notifications: snapshot.notifications,
            home: snapshot.home,
            default_home: snapshot.default_home,
            // Changing the data directory is a first-run decision: doing it
            // later would orphan the profile the user already has.
            can_change_data_dir: !snapshot.first_run_completed,
        }
    }

    /// Persist the wizard's answers; the waiting supervisor thread picks the
    /// `first_run_completed` flag up on its next poll and launches.
    pub fn apply_first_run(&self, choices: FirstRunChoices) -> Result<FirstRunOutcome, String> {
        let locale = match choices.locale.trim() {
            "" => self.shell_settings().locale,
            "en" => "en".to_string(),
            "zh" | "zh-CN" | "zh-Hans" => "zh-CN".to_string(),
            other => return Err(format!("不支持的语言：{other}")),
        };

        let requested = choices
            .data_dir
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);
        let mut restart_required = false;
        let mut settings_home: Option<PathBuf> = None;
        match requested {
            Some(target) => {
                if !target.is_absolute() {
                    return Err("数据目录必须是绝对路径".to_string());
                }
                fs::create_dir_all(&target)
                    .map_err(|error| format!("无法创建数据目录 {}：{error}", target.display()))?;
                if target != self.config.home {
                    Bootstrap::write_home(&self.config.default_home, &target)?;
                    restart_required = true;
                    settings_home = Some(target);
                } else {
                    // "Keep the default" must also undo an earlier pointer.
                    Bootstrap::clear(&self.config.default_home)?;
                }
            }
            None => {
                if self.config.home != self.config.default_home {
                    Bootstrap::clear(&self.config.default_home)?;
                    restart_required = true;
                }
            }
        }

        let snapshot = self.apply_settings_patch(SettingsPatch {
            close_to_tray: Some(choices.close_to_tray),
            notifications: Some(choices.notifications),
            locale: Some(locale),
            first_run_completed: Some(true),
            pack_catalog: None,
        })?;
        // A relaunch will read settings from the *new* home; write the same
        // answers there so the wizard does not reappear after the restart.
        if let Some(home) = settings_home {
            let settings = self
                .settings
                .lock()
                .map_err(|_| "设置不可用（锁已损坏）".to_string())?
                .clone();
            settings.save(&home)?;
        }
        if restart_required {
            self.restart_required.store(true, Ordering::SeqCst);
        }
        self.append_shell_log(&format!(
            "first-run wizard completed (home={}, restart_required={restart_required})",
            self.config.home.display()
        ));
        Ok(FirstRunOutcome {
            restart_required,
            settings: snapshot,
        })
    }

    /// Queue a URL the OS asked us to open and bring the window forward.
    pub fn push_open_urls<'a, I: IntoIterator<Item = &'a tauri::Url>>(
        &self,
        urls: I,
        source: &'static str,
    ) -> usize {
        let accepted = self.handoffs.push_urls(urls, source);
        if accepted > 0 {
            self.append_shell_log(&format!("queued {accepted} open request(s) from {source}"));
            self.notify_ui_of_handoff();
        }
        accepted
    }

    pub fn push_open_args<I: IntoIterator<Item = String>>(
        &self,
        args: I,
        source: &'static str,
    ) -> usize {
        let accepted = self.handoffs.push_args(args, source);
        if accepted > 0 {
            self.append_shell_log(&format!("queued {accepted} open request(s) from {source}"));
            self.notify_ui_of_handoff();
        }
        accepted
    }

    /// Tell the UI there is something waiting, and make sure it is visible.
    fn notify_ui_of_handoff(&self) {
        if self.handoffs.is_empty() {
            return;
        }
        self.emit_to_ui("deeptutor://open-request", serde_json::json!({}));
    }

    /// Deliver the session a notification pointed at, now that the user is back.
    pub fn flush_notification_target(&self) -> Option<NotificationTarget> {
        let target = self.notifications.take()?;
        self.emit_to_ui(
            "deeptutor://notification-target",
            serde_json::to_value(&target).unwrap_or(serde_json::Value::Null),
        );
        Some(target)
    }

    fn emit_to_ui(&self, event: &str, payload: serde_json::Value) {
        let Some(app) = self.app.get().cloned() else {
            return;
        };
        use tauri::Emitter as _;
        if let Err(error) = app.emit(event, payload) {
            self.append_shell_log(&format!("emit {event} failed: {error}"));
        }
    }

    /// Runtime-pack update check. Installs when the catalog has something newer.
    ///
    /// The shell plane is deliberately absent here: it lives in
    /// `tauri-plugin-deeptutor`, which owns the updater plugin, and the caller
    /// combines both into one [`UpdateReport`].
    pub fn check_runtime_updates(&self) -> RuntimeUpdateReport {
        let installer = PackInstaller::new(&self.config.home);
        let source = self.update_source();

        match source {
            None => {
                let state = installer.state();
                RuntimeUpdateReport {
                    checked: false,
                    source: None,
                    updated: false,
                    app_version: state
                        .active_pack
                        .as_ref()
                        .and_then(|pack_id| installer.load(pack_id).ok())
                        .map(|pack| pack.manifest.app_version),
                    active_pack: state.active_pack,
                    previous_pack: state.previous_pack,
                    detail: "未配置运行时更新源：在 desktop/shell.json 写入 pack_catalog，或设置 DEEPTUTOR_DESKTOP_PACK_CATALOG。".to_string(),
                }
            }
            Some(source) => match installer.update_from_catalog(&source) {
                Ok(Some(pack)) => {
                    let state = installer.state();
                    self.append_shell_log(&format!(
                        "runtime pack updated to {} from {source}",
                        pack.pack_id
                    ));
                    let report = RuntimeUpdateReport {
                        checked: true,
                        source: Some(source),
                        updated: true,
                        app_version: Some(pack.manifest.app_version.clone()),
                        active_pack: state.active_pack,
                        previous_pack: state.previous_pack,
                        detail: format!("已安装运行时包 {}", pack.pack_id),
                    };
                    // A new pack only takes effect on the next launch of the
                    // backend, so do that now instead of leaving the user on a
                    // version the UI just said was replaced.
                    if let Err(error) = self.restart() {
                        self.append_shell_log(&format!(
                            "restart after pack update failed: {error}"
                        ));
                    }
                    report
                }
                Ok(None) => {
                    let state = installer.state();
                    RuntimeUpdateReport {
                        checked: true,
                        source: Some(source),
                        updated: false,
                        app_version: state
                            .active_pack
                            .as_ref()
                            .and_then(|pack_id| installer.load(pack_id).ok())
                            .map(|pack| pack.manifest.app_version),
                        active_pack: state.active_pack,
                        previous_pack: state.previous_pack,
                        detail: "运行时包已是最新".to_string(),
                    }
                }
                Err(error) => {
                    let state = installer.state();
                    RuntimeUpdateReport {
                        checked: true,
                        source: Some(source),
                        updated: false,
                        app_version: None,
                        active_pack: state.active_pack,
                        previous_pack: state.previous_pack,
                        detail: format!("检查运行时包失败：{error}"),
                    }
                }
            },
        }
    }

    /// Where the runtime-pack check looks for its catalog, if anywhere.
    fn update_source(&self) -> Option<String> {
        self.shell_settings()
            .pack_catalog
            .or_else(|| std::env::var("DEEPTUTOR_DESKTOP_PACK_CATALOG").ok())
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    }

    /// Read-only variant of [`Supervisor::check_runtime_updates`].
    ///
    /// Answers "is there something newer" without downloading a pack and
    /// without restarting the local service — what a headless gate
    /// (`--check-updates`) must do, and what the tray dialog could report
    /// before spending 250 MB of somebody's bandwidth.
    pub fn preview_runtime_updates(&self, source: Option<String>) -> RuntimeUpdateReport {
        let installer = PackInstaller::new(&self.config.home);
        let source = source.or_else(|| self.update_source());
        let state = installer.state();
        match source {
            None => RuntimeUpdateReport {
                checked: false,
                source: None,
                updated: false,
                app_version: state
                    .active_pack
                    .as_ref()
                    .and_then(|pack_id| installer.load(pack_id).ok())
                    .map(|pack| pack.manifest.app_version),
                active_pack: state.active_pack,
                previous_pack: state.previous_pack,
                detail: "未配置运行时更新源：在 desktop/shell.json 写入 pack_catalog，或设置 DEEPTUTOR_DESKTOP_PACK_CATALOG。".to_string(),
            },
            Some(source) => match installer.outdated_from_catalog(&source) {
                Ok(Some(release)) => RuntimeUpdateReport {
                    checked: true,
                    source: Some(source),
                    updated: false,
                    app_version: state
                        .active_pack
                        .as_ref()
                        .and_then(|pack_id| installer.load(pack_id).ok())
                        .map(|pack| pack.manifest.app_version),
                    active_pack: state.active_pack,
                    previous_pack: state.previous_pack,
                    detail: format!(
                        "有新版本 {} 可用（{} MB）",
                        release.app_version,
                        release.size / (1024 * 1024)
                    ),
                },
                Ok(None) => RuntimeUpdateReport {
                    checked: true,
                    source: Some(source),
                    updated: false,
                    app_version: state
                        .active_pack
                        .as_ref()
                        .and_then(|pack_id| installer.load(pack_id).ok())
                        .map(|pack| pack.manifest.app_version),
                    active_pack: state.active_pack,
                    previous_pack: state.previous_pack,
                    detail: "运行时包已是最新".to_string(),
                },
                Err(error) => RuntimeUpdateReport {
                    checked: true,
                    source: Some(source),
                    updated: false,
                    app_version: None,
                    active_pack: state.active_pack,
                    previous_pack: state.previous_pack,
                    detail: format!("检查运行时包失败：{error}"),
                },
            },
        }
    }
}

/// Language the wizard preselects, from the OS locale.
fn default_locale() -> String {
    for key in ["LANG", "LC_ALL", "LC_MESSAGES"] {
        if let Ok(value) = std::env::var(key) {
            if value.to_ascii_lowercase().starts_with("zh") {
                return "zh-CN".to_string();
            }
            if !value.trim().is_empty() {
                return "en".to_string();
            }
        }
    }
    "zh-CN".to_string()
}

impl Supervisor {
    /// Current main-window geometry, when the window exists.
    ///
    /// Reported because "the window forgot its size" is otherwise invisible in
    /// a log, and because the restore path is hand-rolled (see `window.rs`).
    fn window_geometry(&self) -> Option<WindowGeometry> {
        let guard = self.window.lock().ok()?;
        let window = guard.as_ref()?;
        let size = window.outer_size().ok()?;
        let position = window.outer_position().unwrap_or_default();
        Some(WindowGeometry {
            width: size.width,
            height: size.height,
            x: position.x,
            y: position.y,
            maximized: window.is_maximized().unwrap_or(false),
            fullscreen: window.is_fullscreen().unwrap_or(false),
            visible: window.is_visible().unwrap_or(false),
        })
    }

    /// Cached resolution when known, otherwise the most specific candidate.
    /// Never probes: the status command is polled by the UI.
    fn interpreter_for_display(&self) -> String {
        if let Ok(cache) = self.interpreter.lock() {
            if let Some(candidate) = cache.as_ref() {
                return candidate.path.to_string_lossy().into_owned();
            }
        }
        self.config
            .interpreter_candidates()
            .first()
            .map(|candidate| candidate.path.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
}

#[cfg(unix)]
fn signal_group(child: &Child, terminate: bool) {
    let signal = if terminate {
        libc::SIGTERM
    } else {
        libc::SIGKILL
    };
    // The launcher was spawned with process_group(0), so its pid is the pgid.
    unsafe {
        libc::kill(-(child.id() as i32), signal);
    }
}

#[cfg(windows)]
fn signal_group(child: &Child, _terminate: bool) {
    // taskkill /T is the only portable way to reach the launcher's own children
    // (uvicorn + Node); console processes get no CTRL_BREAK from here, so both
    // the graceful and the forced path use /F.
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

/// Self-test evaluated inside the loopback page.
///
/// Reports through `location.hash` because the interesting failure is "the
/// bridge is missing or the permission was refused", which IPC itself cannot
/// describe. `__deeptutorAllowRestart` is set by the caller in the same eval.
const PROBE_SCRIPT: &str = concat!(
    "(function () {",
    " var report = function (text) {",
    "  try { location.hash = 'deeptutorProbe=' + String(text).replace(/[^A-Za-z0-9_.:;=\\-]/g, '_'); } catch (e) {}",
    " };",
    " if (!window.__TAURI__ || !window.__TAURI__.core) { report('no-bridge'); return; }",
    " var parts = [];",
    " var expect = window.__deeptutorAllowRestart ? 4 : 3;",
    " var done = function () { if (parts.length >= expect) { report(parts.join(' ; ')); } };",
    " var push = function (label, value) { parts.push(label + '=' + value); done(); };",
    " var invoke = window.__TAURI__.core.invoke;",
    " invoke('plugin:app|version').then(",
    "  function (value) { push('core-app-ok', value); },",
    "  function (error) { push('core-app-error', error); });",
    " invoke('plugin:deeptutor|desktop_status').then(",
    "  function (value) { push('shell-cmd-ok', value && (value.launcher_running + ' launch=' + value.launch_count + ' restarts=' + value.restarts + ' win=' + (value.window ? value.window.width + 'x' + value.window.height : '-'))); },",
    "  function (error) { push('shell-cmd-error', error); });",
    // Phase 3 commands, called from the *loopback* document: Tauri's ACL only
    // covers plugin commands, so this is the check that the new permissions are
    // granted to the origin the real UI actually runs on.
    " invoke('plugin:deeptutor|shell_settings').then(",
    "  function (value) { push('shell-settings-ok', value && (value.close_to_tray + '/' + value.notifications + '/' + (value.locale || 'auto'))); },",
    "  function (error) { push('shell-settings-error', error); });",
    " if (window.__deeptutorAllowRestart) {",
    "  invoke('plugin:deeptutor|restart_service').then(",
    "   function () { push('restart', 'ok'); },",
    "   function (error) { push('restart-error', error); });",
    " }",
    "})();"
);

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect()
    }

    fn config_with(pairs: &[(&str, &str)]) -> ShellConfig {
        let map = env_map(pairs);
        ShellConfig::resolve_with(&|key| map.get(key).cloned())
    }

    #[test]
    fn explicit_home_wins_and_owns_the_state_layout() {
        let config = config_with(&[
            ("DEEPTUTOR_HOME", "/tmp/deeptutor"),
            ("HOME", "/Users/example"),
        ]);
        assert_eq!(config.home, PathBuf::from("/tmp/deeptutor"));
        assert_eq!(
            config.state_path,
            PathBuf::from("/tmp/deeptutor/desktop/runtime.json")
        );
        assert_eq!(
            config.logs_dir,
            PathBuf::from("/tmp/deeptutor/desktop/logs")
        );
        // Workdir follows home unless the shell is pointed at a checkout.
        assert_eq!(config.workdir, PathBuf::from("/tmp/deeptutor"));
    }

    #[test]
    fn workdir_override_is_honoured() {
        let config = config_with(&[
            ("DEEPTUTOR_HOME", "/tmp/deeptutor"),
            ("DEEPTUTOR_DESKTOP_WORKDIR", "/src/DeepTutor"),
        ]);
        assert_eq!(config.workdir, PathBuf::from("/src/DeepTutor"));
        assert_eq!(config.home, PathBuf::from("/tmp/deeptutor"));
    }

    #[test]
    fn blank_environment_values_fall_through() {
        let config = config_with(&[("DEEPTUTOR_HOME", "   "), ("HOME", "/Users/example")]);
        assert!(config.home.ends_with("DeepTutor"));
    }

    #[test]
    fn platform_default_home_matches_documented_location() {
        let config = config_with(&[("HOME", "/Users/example")]);
        if cfg!(target_os = "macos") {
            assert_eq!(
                config.home,
                PathBuf::from("/Users/example/Library/Application Support/DeepTutor")
            );
        } else if !cfg!(windows) {
            assert_eq!(
                config.home,
                PathBuf::from("/Users/example/.local/share/DeepTutor")
            );
        }
    }

    #[test]
    fn bundled_venv_is_preferred_over_path_lookup() {
        let config = config_with(&[("DEEPTUTOR_HOME", "/tmp/deeptutor")]);
        let candidates = config.interpreter_candidates();
        assert!(candidates[0].path.to_string_lossy().contains(".venv"));
        assert_eq!(candidates[0].source, "<home>/.venv");
        // The last resort is whatever is on PATH; `resolve_interpreter` probes
        // it before it is ever used.
        assert_eq!(candidates.last().unwrap().source, "PATH");
    }

    #[test]
    fn workdir_venv_is_tried_after_the_app_data_venv() {
        let config = config_with(&[
            ("DEEPTUTOR_HOME", "/tmp/deeptutor"),
            ("DEEPTUTOR_DESKTOP_WORKDIR", "/src/DeepTutor"),
        ]);
        let sources: Vec<&str> = config
            .interpreter_candidates()
            .iter()
            .map(|candidate| candidate.source)
            .collect();
        assert_eq!(sources[0], "<home>/.venv");
        assert_eq!(sources[1], "<workdir>/.venv");
        assert_eq!(sources[2], "PATH");
    }

    #[test]
    fn workdir_equal_to_home_does_not_duplicate_candidates() {
        let config = config_with(&[("DEEPTUTOR_HOME", "/tmp/deeptutor")]);
        let sources: Vec<&str> = config
            .interpreter_candidates()
            .iter()
            .map(|candidate| candidate.source)
            .collect();
        assert_eq!(sources, vec!["<home>/.venv", "PATH", "PATH"]);
    }

    #[test]
    fn explicit_interpreter_is_the_first_candidate() {
        let config = config_with(&[
            ("DEEPTUTOR_HOME", "/tmp/deeptutor"),
            ("DEEPTUTOR_DESKTOP_PYTHON", "/opt/py/bin/python3"),
        ]);
        let candidates = config.interpreter_candidates();
        assert_eq!(candidates[0].source, "DEEPTUTOR_DESKTOP_PYTHON");
        assert_eq!(candidates[0].path, PathBuf::from("/opt/py/bin/python3"));
        assert!(candidates[0].must_exist);
    }

    fn temp_home(name: &str) -> PathBuf {
        let home = std::env::temp_dir().join(format!(
            "deeptutor-supervisor-test-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&home).expect("temp home");
        home
    }

    #[test]
    fn a_bootstrap_pointer_moves_the_data_directory() {
        let default = PathBuf::from("/Users/example/Library/Application Support/DeepTutor");
        let chosen = PathBuf::from("/Volumes/Data/DeepTutor");
        let map = env_map(&[("HOME", "/Users/example")]);
        let config = ShellConfig::resolve_full(&|key| map.get(key).cloned(), &|home: &Path| {
            (home == default).then(|| chosen.clone())
        });
        assert_eq!(config.home, chosen);
        // The pointer's own location never follows the pointer.
        assert_eq!(config.default_home, default);
        assert_eq!(
            config.state_path,
            chosen.join("desktop").join("runtime.json")
        );
        assert_eq!(config.workdir, chosen);
    }

    #[test]
    fn a_bootstrap_pointer_at_another_home_is_ignored_for_dev_overrides() {
        // `DEEPTUTOR_HOME` stays the search root for the pointer, so a checkout
        // run cannot be hijacked by a pointer in the real app-data home.
        let map = env_map(&[
            ("DEEPTUTOR_HOME", "/tmp/deeptutor"),
            ("HOME", "/Users/example"),
        ]);
        let config = ShellConfig::resolve_full(&|key| map.get(key).cloned(), &|home: &Path| {
            (home == Path::new("/Users/example")).then(|| PathBuf::from("/Volumes/Data/DeepTutor"))
        });
        assert_eq!(config.home, PathBuf::from("/tmp/deeptutor"));
        assert_eq!(config.default_home, PathBuf::from("/tmp/deeptutor"));
    }

    #[test]
    fn settings_patch_persists_and_gates_notifications() {
        let home = temp_home("patch");
        let config = ShellConfig::resolve_full(
            &|key| (key == "DEEPTUTOR_HOME").then(|| home.to_string_lossy().into_owned()),
            &|_| None,
        );
        let supervisor = Supervisor::new_shared(config);
        assert!(supervisor.shell_settings().notifications);

        let snapshot = supervisor
            .apply_settings_patch(SettingsPatch {
                notifications: Some(false),
                close_to_tray: Some(false),
                locale: Some("en".to_string()),
                ..SettingsPatch::default()
            })
            .expect("patch");
        assert!(!snapshot.notifications);
        assert!(!snapshot.close_to_tray);
        assert_eq!(snapshot.locale, "en");
        assert!(!supervisor.notifications.is_enabled());
        // Persisted, not just in memory.
        let reloaded = ShellSettings::load(&home);
        assert!(!reloaded.notifications);
        assert_eq!(reloaded.locale, "en");

        assert!(supervisor
            .apply_settings_patch(SettingsPatch {
                locale: Some("fr".to_string()),
                ..SettingsPatch::default()
            })
            .is_err());
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn the_first_run_wizard_gates_and_then_releases_the_launcher() {
        let home = temp_home("first-run");
        let config = ShellConfig::resolve_full(
            &|key| (key == "DEEPTUTOR_HOME").then(|| home.to_string_lossy().into_owned()),
            &|_| None,
        );
        let supervisor = Supervisor::new_shared(config);

        let state = supervisor.first_run_state();
        assert!(!state.completed);
        assert!(state.can_change_data_dir);
        assert_eq!(state.home, home.to_string_lossy());
        assert!(supervisor.needs_first_run());

        let outcome = supervisor
            .apply_first_run(FirstRunChoices {
                locale: "en".to_string(),
                data_dir: None,
                close_to_tray: true,
                notifications: false,
            })
            .expect("apply");
        assert!(!outcome.restart_required);
        assert!(outcome.settings.first_run_completed);
        assert!(!outcome.settings.notifications);
        assert!(!supervisor.needs_first_run());
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn choosing_another_data_directory_asks_for_a_restart_and_keeps_the_answers() {
        let home = temp_home("data-dir");
        let target = home.join("elsewhere");
        let config = ShellConfig::resolve_full(
            &|key| (key == "DEEPTUTOR_HOME").then(|| home.to_string_lossy().into_owned()),
            &|_| None,
        );
        let supervisor = Supervisor::new_shared(config);
        let outcome = supervisor
            .apply_first_run(FirstRunChoices {
                locale: "zh-CN".to_string(),
                data_dir: Some(target.to_string_lossy().into_owned()),
                close_to_tray: false,
                notifications: true,
            })
            .expect("apply");
        assert!(outcome.restart_required);
        assert!(supervisor.shell_settings().restart_required);
        // The pointer lives in the default home; the answers travel with it, so
        // the relaunched shell does not ask again.
        assert_eq!(Bootstrap::read_home(&home), Some(target.clone()));
        let reloaded = ShellSettings::load(&target);
        assert!(reloaded.first_run_completed);
        assert!(!reloaded.close_to_tray);
        assert_eq!(reloaded.locale, "zh-CN");
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn a_relative_data_directory_is_refused() {
        let home = temp_home("relative");
        let config = ShellConfig::resolve_full(
            &|key| (key == "DEEPTUTOR_HOME").then(|| home.to_string_lossy().into_owned()),
            &|_| None,
        );
        let supervisor = Supervisor::new_shared(config);
        assert!(supervisor
            .apply_first_run(FirstRunChoices {
                locale: "en".to_string(),
                data_dir: Some("relative/path".to_string()),
                close_to_tray: true,
                notifications: true,
            })
            .is_err());
        // A refused answer must not mark the wizard as done.
        assert!(supervisor.needs_first_run());
        let _ = fs::remove_dir_all(&home);
    }
}
