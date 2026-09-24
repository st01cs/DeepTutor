# Phase 1 进展报告

日期：2026-09-24 · 范围：桌面外壳 MVP（插件化、菜单/托盘、生命周期硬化、CI）

## 0. 结论

Phase 1 的功能面已全部落地并在真实栈上验证：外壳命令插件化（解决 Phase 0 的硬约束）、
菜单/托盘/单实例/窗口状态/原生错误弹窗、崩溃退避自愈与手动重启、以及 `desktop-ci.yml`。

| 项 | 状态 |
| --- | --- |
| `tauri-plugin-deeptutor`（先行项） | ✅ 已验证：loopback 页面可调用 `desktop_status` / `restart_service` |
| 崩溃重启（2s/4s/8s 退避，最多 3 次） | ✅ 已实现（手动重启不计入崩溃） |
| 优雅退出 / 父进程守护 | ✅ Phase 0 已验；Phase 1 保持 |
| 原生菜单 + "打开日志目录" | ✅ 已实现（含 ⌘, / ⌘C/⌘V 依赖的编辑菜单） |
| 托盘 | ✅ 已实现（显示窗口 / 重启服务 / 打开日志 / 退出 + 左键唤回） |
| 单实例 | ✅ 已实现（第二次启动聚焦已有窗口） |
| 窗口状态记忆 | ✅ 已实现（`tauri-plugin-window-state`） |
| 原生错误对话框 | ✅ 已实现（失败时先收摊子进程再弹窗） |
| `desktop` 安装模式（禁用 pip 自更新） | ✅ Phase 0 已落地并测试 |
| CI（macOS arm64：fmt/clippy/test/build/self-check） | ✅ 新增 `desktop-ci.yml` |

## 1. 本阶段产出

**新增**

- `desktop/Cargo.toml`：workspace 根（一个 lockfile / 一个 target 目录）
- `desktop/plugins/tauri-plugin-deeptutor/`：插件 crate（命令 + `permissions/default.toml` + build.rs 生成 `allow-*`）
- `desktop/src-tauri/src/app.rs`：菜单、托盘、菜单事件处理
- `desktop/src-tauri/src/main.rs`：`--self-check` 冒烟入口 + 插件装配
- `.github/workflows/desktop-ci.yml`
- `desktop/.gitignore`

**修改**

- `desktop/src-tauri/src/supervisor.rs`：generation 机制（重启/退出不打架）、崩溃退避重启、
  手动重启、`DesktopBackend` 实现、可注入环境/文件系统的配置解析（便于单测）
- `desktop/src-tauri/Cargo.toml`：新增 dialog / opener / single-instance / window-state 插件，
  `tray-icon` feature
- `desktop/src-tauri/capabilities/*.json`：授予 `deeptutor:default`
- `desktop/web/index.html`：splash 自检改调插件命令

## 2. 运行时验证（macOS 27.0, arm64）

启动外壳后，splash 导航到本地 UI，外壳**从 loopback 页面内部**触发自检并写入
`desktop/logs/shell.log`：

```
[shell] remote-ipc probe dispatched (attempt 1)
[shell] webview-ipc ok: invoke received from http://localhost:3783/chat
[shell] manual restart requested
[shell] remote-ipc probe result: core-app-ok=1.6.10 ; shell-cmd-ok=true launch=1 restarts=0 ; restart=ok
[shell] remote-ipc probe dispatched (attempt 1)
[shell] webview-ipc ok: invoke received from http://localhost:3783/chat
[shell] remote-ipc probe result: core-app-ok=1.6.10 ; shell-cmd-ok=true launch=2 restarts=0
```

逐条解读：

- `shell-cmd-ok=true` —— 插件命令 `plugin:deeptutor|desktop_status` 从远程源调用成功
  （Phase 0 里同样的调用被拒为 `not allowed. Plugin not found`）。
- `restart=ok` —— 插件命令 `restart_service` 从 UI 调用成功。
- `launch=2` —— 服务**真的**重启了一次；`restarts=0` —— 手动重启没有被误判成崩溃
  （这正是 generation 先退休旧监督线程再杀进程的原因）。
- 第二轮自检会自动出现并再次通过 —— 重启后窗口重新握手、重新导航。

退出后 `ps` 无任何 `deeptutor-desktop` / `deeptutor_cli.main` / `uvicorn` / `server.js` 残留。

## 3. 静态检查与测试

```bash
cd desktop
cargo fmt --all -- --check          # OK
cargo clippy --locked --all-targets -- -D warnings   # OK（无警告）
cargo test --locked                 # 8 passed（配置解析：home/workdir 覆盖、空白环境变量、
                                    #            平台默认路径、解释器候选顺序、workdir venv、
                                    #            显式覆盖优先、同目录不重复）
./target/debug/deeptutor-desktop --self-check
```

`--self-check` 输出（用于 CI，无窗口、无 launcher）：

```json
{
  "home": ".../DeepTutor",
  "logs_dir": ".../DeepTutor/desktop/logs",
  "mode": "self-check",
  "python": "python3",
  "shell": "deeptutor-desktop",
  "state_path": ".../DeepTutor/desktop/runtime.json",
  "workdir": "..."
}
```

Python 侧回归：`tests/runtime` + `tests/services/test_app_update.py` 共 **219 passed / 4 skipped**。

## 4. 过程中的三个坑（已修）

1. **`Builder::new` 的类型参数**：`tauri::plugin::Builder<R, C = ()>` 不写 turbofish 时
   整条链会落到默认 runtime，返回类型对不上；`Builder::<R, ()>::new(...)` 解决。
2. **插件命令的运行时泛型**：命令参数不能写 `tauri::WebviewWindow`（默认 runtime），
   必须 `tauri::WebviewWindow<R>`；否则 `CommandArg` 在泛型插件里不成立。
3. **解释器回退到系统 Python（用户报障"启动失败，退出码 1"）**：不设
   `DEEPTUTOR_DESKTOP_PYTHON` 时外壳只找 `<home>/.venv`，找不到就回退 `PATH` 上的
   `python3`——Homebrew 的 3.14 没有项目依赖，launcher 退出码 1，而对话框只显示了这个数字。
   修复：候选链增加 `<workdir>/.venv`；每个候选都用 `import deeptutor_cli.main` 实测；
   全部失败时给出逐条原因 + 三种修复方式，并附 `launcher.log` 末尾几行；`--self-check`
   报告解析出的解释器、来源与候选清单（`--require-python` 可当门禁）。

   验证：只设 `DEEPTUTOR_DESKTOP_WORKDIR=<repo>` 启动 → 日志
   `using interpreter <repo>/.venv/bin/python (from <workdir>/.venv)`，`runtime.json`
   进入 `ready`，端口 8001/3782（用户环境下的默认值）；故意指向无效解释器时 shell.log
   列出五个候选的失败原因并 exit 1。

## 5. 遗留（Phase 2 起）

- 托盘尚未接入"隐藏到托盘而不是退出"的策略（当前关窗即退出；Phase 3 做成设置项）。
- "检查更新"菜单项尚未接入 updater（Phase 2 的 `tauri-plugin-updater`）。
- 应用图标仍是 `assets/figs/logo/logo.png`（543×533）；打包前用 `tauri icon` 生成全套尺寸。
- Windows/Linux 尚未进 CI 矩阵（Phase 2 / v1.x）。
- 四个 WebView 高风险面（PDF / EPUB / 拖拽 / 导出下载）仍需人工点检（Phase 0 §5 清单）。
