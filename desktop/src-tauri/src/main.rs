//! DeepTutor desktop shell.
//!
//! A supervisor thread owns the Python launcher, the window is handed over to
//! the loopback UI once the launcher reports ready, every command the UI may
//! call lives in `tauri-plugin-deeptutor` (Tauri's ACL only grants *plugin*
//! commands to a remote origin), and the interpreter comes from a runtime pack
//! when one is installed.
//!
//! Headless modes (no window) exist for installers, CI and debugging:
//! `--self-check`, `--pack-status`, `--install-pack <file|url>`, `--rollback-pack`.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod runtime_info;
mod runtime_pack;
mod supervisor;

use std::sync::Arc;

use tauri::{Manager, RunEvent};
use tauri_plugin_deeptutor::DesktopBackend;

use runtime_pack::{InstalledPack, PackInstaller};
use supervisor::{ShellConfig, Supervisor};

/// Everything a CI run, an installer script or a debugging session needs,
/// without opening a window.
enum Headless {
    SelfCheck {
        require_python: bool,
    },
    PackStatus,
    PackCatalog {
        source: String,
    },
    InstallPack {
        source: String,
        sha256: Option<String>,
    },
    UpdatePack {
        source: String,
    },
    RollbackPack,
}

fn headless_mode(args: &[String]) -> Option<Headless> {
    let flag = |name: &str| args.iter().any(|arg| arg == name);
    let value = |name: &str| {
        args.iter()
            .position(|arg| arg == name)
            .and_then(|index| args.get(index + 1))
            .cloned()
    };
    if flag("--self-check") {
        return Some(Headless::SelfCheck {
            require_python: flag("--require-python"),
        });
    }
    if flag("--pack-status") {
        return Some(Headless::PackStatus);
    }
    if let Some(source) = value("--pack-catalog") {
        return Some(Headless::PackCatalog { source });
    }
    if flag("--update-pack") {
        return Some(Headless::UpdatePack {
            source: value("--catalog").unwrap_or_default(),
        });
    }
    if flag("--rollback-pack") {
        return Some(Headless::RollbackPack);
    }
    value("--install-pack").map(|source| Headless::InstallPack {
        source,
        sha256: value("--sha256"),
    })
}

fn print_json(value: &serde_json::Value) {
    println!(
        "{}",
        serde_json::to_string_pretty(value).unwrap_or_else(|_| "{}".to_string())
    );
}

fn pack_summary(pack: &InstalledPack) -> serde_json::Value {
    serde_json::json!({
        "pack_id": pack.pack_id,
        "dir": pack.dir,
        "app_version": pack.manifest.app_version,
        "platform": pack.manifest.platform,
        "python": pack.manifest.python_path(&pack.dir),
        "node_dir": pack.manifest.node_dir(&pack.dir),
    })
}

fn run_headless(mode: Headless) -> i32 {
    let config = ShellConfig::resolve();
    match mode {
        Headless::SelfCheck { require_python } => {
            let candidates: Vec<serde_json::Value> = config
                .interpreter_candidates()
                .into_iter()
                .map(|candidate| {
                    serde_json::json!({
                        "path": candidate.path,
                        "source": candidate.source,
                        "exists": candidate.path.exists(),
                    })
                })
                .collect();
            let resolved = config.resolve_interpreter();
            let python_ok = resolved.is_ok();
            print_json(&serde_json::json!({
                "shell": "deeptutor-desktop",
                "mode": "self-check",
                "home": config.home,
                "workdir": config.workdir,
                "state_path": config.state_path,
                "logs_dir": config.logs_dir,
                "active_pack": config.active_pack().map(|pack| pack.pack_id),
                "python": resolved
                    .as_ref()
                    .ok()
                    .map(|candidate| candidate.path.to_string_lossy().into_owned()),
                "python_source": resolved.as_ref().ok().map(|candidate| candidate.source),
                "python_ok": python_ok,
                "python_error": resolved.as_ref().err(),
                "python_candidates": candidates,
            }));
            // `--require-python` turns the diagnostic into a gate.
            i32::from(!python_ok && require_python)
        }

        Headless::PackStatus => {
            let installer = PackInstaller::new(&config.home);
            let state = installer.state();
            let installed: Vec<serde_json::Value> = installer
                .installed()
                .iter()
                .filter_map(|pack_id| installer.load(pack_id).ok())
                .map(|pack| pack_summary(&pack))
                .collect();
            print_json(&serde_json::json!({
                "shell": "deeptutor-desktop",
                "mode": "pack-status",
                "home": config.home,
                "platform": runtime_pack::host_platform(),
                "active_pack": state.active_pack,
                "previous_pack": state.previous_pack,
                "installed": installed,
            }));
            0
        }

        Headless::PackCatalog { source } => {
            let installer = PackInstaller::new(&config.home);
            match installer.catalog(&source) {
                Ok(catalog) => {
                    let host = runtime_pack::host_platform();
                    let available: Vec<serde_json::Value> = catalog
                        .packs
                        .iter()
                        .filter(|release| release.platform == host)
                        .map(|release| {
                            serde_json::json!({
                                "pack_id": release.pack_id,
                                "app_version": release.app_version,
                                "url": release.url,
                                "size": release.size,
                            })
                        })
                        .collect();
                    print_json(&serde_json::json!({
                        "shell": "deeptutor-desktop",
                        "mode": "pack-catalog",
                        "source": source,
                        "platform": host,
                        "active_pack": installer.state().active_pack,
                        "available": available,
                    }));
                    0
                }
                Err(error) => {
                    eprintln!("读取运行时包清单失败: {error}");
                    1
                }
            }
        }

        Headless::UpdatePack { source } => {
            if source.is_empty() {
                eprintln!("--update-pack 需要 --catalog <url|path>");
                return 2;
            }
            let installer = PackInstaller::new(&config.home);
            match installer.update_from_catalog(&source) {
                Ok(Some(pack)) => {
                    let state = installer.state();
                    print_json(&serde_json::json!({
                        "shell": "deeptutor-desktop",
                        "mode": "update-pack",
                        "updated": true,
                        "active_pack": state.active_pack,
                        "previous_pack": state.previous_pack,
                        "pack": pack_summary(&pack),
                    }));
                    0
                }
                Ok(None) => {
                    print_json(&serde_json::json!({
                        "shell": "deeptutor-desktop",
                        "mode": "update-pack",
                        "updated": false,
                        "active_pack": installer.state().active_pack,
                    }));
                    0
                }
                Err(error) => {
                    eprintln!("更新运行时包失败: {error}");
                    1
                }
            }
        }

        Headless::InstallPack { source, sha256 } => {
            let installer = PackInstaller::new(&config.home);
            let result = if source.starts_with("http://") || source.starts_with("https://") {
                match sha256.as_deref() {
                    Some(expected) => installer.install_from_url(&source, expected),
                    None => Err("从 URL 安装必须提供 --sha256".to_string()),
                }
            } else {
                installer.install_archive(std::path::Path::new(&source), sha256.as_deref())
            };
            match result {
                Ok(pack) => {
                    print_json(&serde_json::json!({
                        "shell": "deeptutor-desktop",
                        "mode": "install-pack",
                        "home": config.home,
                        "installed": pack_summary(&pack),
                        "active_pack": installer.state().active_pack,
                    }));
                    0
                }
                Err(error) => {
                    eprintln!("安装运行时包失败: {error}");
                    1
                }
            }
        }

        Headless::RollbackPack => {
            let installer = PackInstaller::new(&config.home);
            match installer.rollback() {
                Ok(pack) => {
                    print_json(&serde_json::json!({
                        "shell": "deeptutor-desktop",
                        "mode": "rollback-pack",
                        "active_pack": installer.state().active_pack,
                        "rolled_back_to": pack_summary(&pack),
                    }));
                    0
                }
                Err(error) => {
                    eprintln!("回滚失败: {error}");
                    1
                }
            }
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if let Some(mode) = headless_mode(&args) {
        std::process::exit(run_headless(mode));
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
