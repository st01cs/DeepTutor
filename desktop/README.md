# DeepTutor Desktop（Phase 0 骨架）

Tauri v2 外壳，负责三件事：把 Python launcher 拉起来、等它的就绪握手、把窗口交给本地
Next.js 服务。业务逻辑（端口、设置、前端构建、更新交接）全部留在
`deeptutor/runtime/launcher.py`，所以 Web 与 CLI 模式不受影响。

完整方案见 [`../docs-for-user/DESKTOP_TAURI_PLAN.md`](../docs-for-user/DESKTOP_TAURI_PLAN.md)，
当前进展与待验证项见 [`PHASE0_REPORT.md`](PHASE0_REPORT.md)。

## 目录

| 路径 | 作用 |
| --- | --- |
| `src-tauri/src/main.rs` | 窗口 + 应用生命周期 + `desktop_probe` 自检命令 |
| `src-tauri/src/supervisor.rs` | 拉起/守护 Python launcher、就绪轮询、退出清理 |
| `src-tauri/src/runtime_info.rs` | 读取 launcher 的 `--runtime-info` 状态文件 |
| `src-tauri/capabilities/` | `main.json`（本窗口）+ `remote-web.json`（本地 UI 的 IPC 授权） |
| `web/` | splash 页面（内嵌，不需要 Node 构建） |
| `pack/`、`scripts/` | Phase 2 的运行时包与版本同步（尚未创建） |

## 运行（Phase 0）

前置：Rust 工具链、Python 依赖、`web/node_modules`（`deeptutor start` 会自动 `npm ci` 与
生产构建，首次较慢）。

```bash
export DEEPTUTOR_HOME="$HOME/Library/Application Support/DeepTutor"
export DEEPTUTOR_DESKTOP_WORKDIR="$PWD"
export DEEPTUTOR_DESKTOP_PYTHON="$PWD/.venv/bin/python"

cd desktop/src-tauri
cargo tauri dev            # 或：npx --yes @tauri-apps/cli@^2 dev
```

可用环境变量：

| 变量 | 默认 | 说明 |
| --- | --- | --- |
| `DEEPTUTOR_HOME` | macOS `~/Library/Application Support/DeepTutor` | 运行时目录（`data/`、`desktop/`） |
| `DEEPTUTOR_DESKTOP_WORKDIR` | 同 `DEEPTUTOR_HOME` | launcher 的工作目录；Phase 0 指向仓库根 |
| `DEEPTUTOR_DESKTOP_PYTHON` | `<home>/.venv/bin/python` → `python3` | launcher 解释器 |

外壳传给 launcher 的参数（全部是可选加法）：

```bash
python -m deeptutor_cli.main start \
  --home <home> --no-browser --auto-ports \
  --runtime-info <home>/desktop/runtime.json \
  --parent-pid <shell pid>
```

`DEEPTUTOR_DESKTOP_SHELL=1` 由外壳设置，作用是让 `detect_installation()` 返回
`desktop` 模式，从而禁用 pip 自更新（签名过的应用包不能自我改写）。

## 版本

`src-tauri/Cargo.toml` 与 `src-tauri/tauri.conf.json` 的 `version` 必须与
`deeptutor/__version__.py` 一致。Phase 2 的 `desktop/scripts/sync_version.py` 会把它变成
自动同步 + 发布守卫测试，在此之前请手工同步。
