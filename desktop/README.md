# DeepTutor Desktop（Tauri v2）

桌面外壳负责三件事：把 Python launcher 拉起来并守护它、等它的就绪握手、把窗口交给本地
Next.js 服务；外加原生菜单、托盘、单实例、窗口状态与错误弹窗。业务逻辑（端口、设置、
前端构建、更新交接）全部留在 `deeptutor/runtime/launcher.py`，所以 Web 与 CLI 模式不受影响。

完整方案见 [`../docs-for-user/DESKTOP_TAURI_PLAN.md`](../docs-for-user/DESKTOP_TAURI_PLAN.md)；
分阶段验证记录见 [`PHASE0_REPORT.md`](PHASE0_REPORT.md)、
[`PHASE1_REPORT.md`](PHASE1_REPORT.md) 与 [`PHASE2_REPORT.md`](PHASE2_REPORT.md)。

## 结构

| 路径 | 作用 |
| --- | --- |
| `Cargo.toml` | workspace 根（一个 lockfile、一个 target 目录） |
| `src-tauri/src/main.rs` | 进程入口、插件装配、`--self-check` 冒烟模式 |
| `src-tauri/src/app.rs` | 菜单栏、托盘与它们的动作 |
| `src-tauri/src/supervisor.rs` | 拉起/守护/重启 Python launcher、就绪握手、退出清理 |
| `src-tauri/src/runtime_info.rs` | 读取 launcher 的 `--runtime-info` 状态文件 |
| `plugins/tauri-plugin-deeptutor/` | 外壳命令（供 UI 调用）+ permission set |
| `src-tauri/capabilities/` | `main.json`（本窗口）+ `remote-web.json`（本地 UI 的 IPC 授权） |
| `web/` | splash 页面（内嵌，不需要 Node 构建） |
| `scripts/phase0_*.sh` | 不依赖 Tauri 的握手 / 孤儿守卫验证脚本 |
| `pack/` | 运行时包：`runtime.lock.txt`、`build_pack.py`、`assemble_catalog.py` |
| `scripts/sync_version.py` | 版本同步与 `--check` 守卫 |

## 运行时包（Phase 2）

包 = 自带 CPython + 内含 DeepTutor 的可重定位 venv + Node + 前端产物。装好之后用户机器上
**不需要任何 Python/Node/环境变量**。

```bash
# 构建（必须在目标平台上跑；CI 用 macos-14 / macos-13 / windows-2022 矩阵）
python3 desktop/pack/build_pack.py                 # 产出 dist/*.tar.gz + .sha256 + .catalog.json
python3 desktop/pack/assemble_catalog.py           # 合并成 runtime-packs.json

# 安装 / 查看 / 更新 / 回滚（无头，不需要开窗口）
deeptutor-desktop --install-pack dist/1.6.10-macos-aarch64.tar.gz --sha256 <digest>
deeptutor-desktop --pack-status
deeptutor-desktop --pack-catalog https://…/runtime-packs.json
deeptutor-desktop --update-pack --catalog https://…/runtime-packs.json
deeptutor-desktop --rollback-pack
```

装好的包放在 `<home>/runtimes/<pack_id>/`，当前使用与上一个包记录在
`<home>/desktop/state.json`；解释器候选链里**运行时包排在 app-data venv 之前**（显式
`DEEPTUTOR_DESKTOP_PYTHON` 仍然优先）。

安装时会做四件事：校验 sha256（不匹配直接拒绝，不落盘）→ 安全解包（拒绝绝对路径与 `..`）
→ rehydrate（把 venv 里指向构建机的绝对路径改成这台机器的）→ 冒烟（`import deeptutor_cli.main,
deeptutor_web`）。

## 为什么外壳命令是"插件"

UI 由 loopback 上的 Next 服务提供，相对 Tauri 属于**远程源**。Phase 0 实测：

```
core-app-ok=1.6.10 ; app-cmd-error=desktop_probe not allowed. Plugin not found
```

- core / 插件命令可以从 loopback 页面调用（`http://127.0.0.1:*` 通配授权有效）；
- **应用自定义命令（`generate_handler!`）不带 ACL 条目，远程源无法调用**。

所以 `plugin:deeptutor|desktop_status`、`plugin:deeptutor|restart_service` 都在
`plugins/tauri-plugin-deeptutor` 里，并在 capability 中授予 `deeptutor:default`。
需要被 UI 调用的新能力，继续往这个插件里加，不要再加应用命令。

## 构建与运行

```bash
# 只检查外壳本身
cd desktop
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --locked

# 冒烟：解析运行时目录并打印，不开窗口、不起 launcher
./target/debug/deeptutor-desktop --self-check

# 真正跑起来（Phase 1 直接指向源码 checkout）
export DEEPTUTOR_HOME="$HOME/Library/Application Support/DeepTutor"
export DEEPTUTOR_DESKTOP_WORKDIR="/path/to/DeepTutor"
export DEEPTUTOR_DESKTOP_PYTHON="/path/to/DeepTutor/.venv/bin/python"
./target/debug/deeptutor-desktop
```

| 变量 | 默认 | 说明 |
| --- | --- | --- |
| `DEEPTUTOR_HOME` | macOS `~/Library/Application Support/DeepTutor` | 运行时目录（`data/`、`desktop/`） |
| `DEEPTUTOR_DESKTOP_WORKDIR` | 同 `DEEPTUTOR_HOME` | launcher 的工作目录；Phase 1 指向仓库根 |
| `DEEPTUTOR_DESKTOP_PYTHON` | 见下节 | 显式指定 launcher 解释器 |

## 解释器与运行环境

外壳按顺序尝试这些解释器，并**逐个用 `import deeptutor_cli.main` 实测**，取第一个真的能用的：

1. `DEEPTUTOR_DESKTOP_PYTHON`（显式覆盖）
2. `<home>/.venv/bin/python`（Phase 2 运行时包的位置）
3. `<workdir>/.venv/bin/python`（开发时仓库里的虚拟环境）
4. `PATH` 上的 `python3` / `python`

所以从仓库开发时，**只要设 `DEEPTUTOR_DESKTOP_WORKDIR` 就够了**：

```bash
DEEPTUTOR_DESKTOP_WORKDIR=/path/to/DeepTutor ./desktop/target/debug/deeptutor-desktop
```

一个候选都没有通过时，外壳不会只说"退出码 1"，而是把每个候选的失败原因和三种修复方式写进日志、
splash 与原生错误弹窗；launcher 启动失败时还会附上 `launcher.log` 的最后几行。

排查入口：

```bash
./target/debug/deeptutor-desktop --self-check                  # 打印解析结果，始终 exit 0
./target/debug/deeptutor-desktop --self-check --require-python  # 没解析到可用环境就 exit 1
```

外壳传给 launcher 的参数（全部是 Phase 0 落地的可选加法）：

```bash
python -m deeptutor_cli.main start \
  --home <home> --no-browser --auto-ports \
  --runtime-info <home>/desktop/runtime.json \
  --parent-pid <shell pid>
```

`DEEPTUTOR_DESKTOP_SHELL=1` 由外壳设置，让 `detect_installation()` 返回 `desktop` 模式从而
禁用 pip 自更新（签名过的应用包不能自我改写）。

## 已实现的桌面能力（Phase 1）

- **菜单栏**：DeepTutor / 编辑（⌘C/⌘V 在 WebView 内生效）/ 视图（重新加载、全屏、调试版
  开发者工具）/ 窗口 / 帮助；含"设置 ⌘,、重新启动本地服务、打开日志目录、使用文档"。
- **托盘**：显示主窗口、重新启动本地服务、打开日志目录、退出；左键点击唤回窗口。
- **单实例**：第二次启动只聚焦已有窗口。
- **窗口状态**：尺寸/位置由 `tauri-plugin-window-state` 记忆。
- **失败可见**：启动失败写 `desktop/logs/shell.log` + splash 红字 + 原生错误弹窗，并先收摊
  子进程，绝不留下半启动的 backend/frontend。
- **崩溃自愈**：launcher 异常退出时按 2s/4s/8s 退避自动重启，最多 3 次；手动重启不会被
  计成崩溃（generation 先退休旧线程再杀进程）。

## 版本

`src-tauri/Cargo.toml`、`plugins/tauri-plugin-deeptutor/Cargo.toml` 与
`src-tauri/tauri.conf.json` 的 `version` 必须与 `deeptutor/__version__.py` 一致。
Phase 2 的 `desktop/scripts/sync_version.py` 会把它变成自动同步 + 发布守卫，在此之前手工同步。

## CI

`.github/workflows/desktop-ci.yml` 在 `desktop/**` 变更时跑 fmt / clippy / 单测 / 构建 /
`--self-check` 冒烟（macOS arm64）。Python 侧的桌面契约由
`tests/runtime/test_desktop_launcher_contract.py` 通过 `tests.yml` 覆盖。
