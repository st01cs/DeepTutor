# Phase 0 进展报告

日期：2026-09-24 · 范围：Tauri v2 桌面外壳的技术验证

## 0. 结论（TL;DR）

| 判断 | 结果 |
| --- | --- |
| Tauri 外壳能否编译并接管现有栈 | ✅ 能。窗口在 launcher 就绪后跳到本地 UI，Web/CLI 路径未改动 |
| 握手 / 自动端口 / 孤儿守卫 | ✅ 真实栈上全部通过（热启动 3s，强杀父进程 3s 内收敛） |
| 本地 UI 能否调用外壳（远程 IPC） | ✅ 可以，**但有硬约束**：只有"插件命令"能授权给远程源，应用自定义命令不行 |
| 四个 WebView 高风险面 | ⛔ 仍需人工点一遍（§5） |
| 过程中发现的产品缺陷 | 🐞 1 个真实 bug，已修复（§6） |

## 1. 环境（已补齐）

| 项 | 结果 |
| --- | --- |
| OS / 架构 | macOS 27.0 (Build 26A428), arm64 |
| Rust | 1.98.1（rustup minimal profile，首次编译 27.9s） |
| Python | 3.12.13（`.venv`，160 个包，`pip install -e ".[cli,server]"`） |
| Node / npm | v24.14.0 / 945 个包（`npm ci --legacy-peer-deps`） |
| 测试工具 | pytest 9.1.1 + pytest-asyncio 1.4.0 |
| 运行时数据 | `.phase0-home/`（仓库内，专用，未触碰 `~/Library/Application Support`） |

## 2. 本阶段产出

**新增：**

- `desktop/src-tauri/`：Tauri v2 工程（`tauri.conf.json`、`build.rs`、`Cargo.toml`、两个 capability）
- `desktop/src-tauri/src/{main,supervisor,runtime_info}.rs`
- `desktop/web/index.html` + `logo.png`（内嵌 splash，无需 Node 构建）
- `desktop/src-tauri/icons/icon.png`
- `desktop/scripts/phase0_handshake_check.sh`（V2/V8/V10 自动验证）
- `desktop/scripts/phase0_orphan_check.sh`（V9/V11 自动验证）
- `tests/runtime/test_desktop_launcher_contract.py`（10 条用例）

**修改（纯加法，默认行为不变）：**

- `deeptutor/runtime/launcher.py`：`--runtime-info` / `--auto-ports` / `--parent-pid`、`RuntimeInfoWriter`、`_auto_resolve_ports()`、父进程看门狗
- `deeptutor_cli/main.py`：`start` 透传三个新选项
- `deeptutor/services/app_update.py`：新增 `desktop` 安装模式（禁用 pip 自更新）
- `deeptutor/runtime/banner.py`：两条 i18n 文案

**测试**：`tests/runtime` 208 passed / 4 skipped（含新增 10 条）。

## 3. 验证结果

| # | 项目 | 结果 | 证据 |
| --- | --- | --- | --- |
| V1 | 外壳编译 | ✅ | `cargo build` 27.9s；`gen/schemas/capabilities.json` 记录了 `remote.urls`，说明 config 与 capability 均通过校验 |
| V2 | 握手 + 窗口接管 | ✅ | `runtime.json` → `status=ready`；`shell.log` → `remote-ipc probe dispatched`；`launcher.log` → `Frontend is ready` |
| V3 | 远程 IPC | ✅ 有条件通过 | 从 `http://localhost:3783` 调用 `plugin:app\|version` 返回 `1.6.10`（含 `http://127.0.0.1:*` 通配 grant）；同一页面调用应用命令 `desktop_probe` 返回 `not allowed. Plugin not found` |
| V4–V7 | PDF / EPUB / 拖拽 / 导出下载 | ⛔ 待人工 | 需要人在窗口里点，见 §5 |
| V8 | 正常退出无残留 | ✅ | 握手脚本：SIGTERM 后 launcher 退出、`pgrep deeptutor.api.main` 为空、状态 `stopped` |
| V9 | 强杀父进程 | ✅ | 孤儿脚本：`kill -9` 父进程后 3s 内 launcher 与两个子进程全部退出，无残留 |
| V10 | 端口冲突自动迁移 | ✅ | 先占用 8001/3782 → 启动后自动改用 8002/3783，并写入 `data/user/settings/system.json` |
| V11 | 体积/耗时基线 | ✅ | 冷启 **71s**（含首次前端生产构建）；热启 **3s**。体积基线：`.venv` 492MB、前端 `standalone/` 86MB、debug 外壳 34MB（release 预计 10–15MB）、`node_modules` 973MB（仅构建期，不入包）→ 运行时包粗估 600–650MB 未压缩（Node 20 约 +50MB），压缩后约 300MB，与"300–600MB 离线胖包"的决策相符 |

## 4. 远程 IPC 的设计约束（本阶段最重要的发现）

实验（同一页面、同一 capability、两次构建对照）：

```
core-app-ok=1.6.10 ; app-cmd-error=desktop_probe not allowed. Plugin not found
```

- `remote.urls` 的 `http://127.0.0.1:*` / `http://localhost:*` **确实生效**（显式端口与通配两种写法都放行了 core 插件命令）→ 端口可以继续浮动，不需要固定端口池。
- **Tauri 应用自定义命令（`generate_handler!`）不带 ACL 条目，远程源无法调用**。
- 因此阶段一的桥接方案确定为：**把 shell 侧需要被 UI 调用的能力做成自定义 Tauri 插件**（带 permission set），而不是应用命令；OS 级能力（通知/对话框/opener/updater/剪贴板/窗口）本身就是插件，远程可用，不需要改造后端。

这同时否掉了"后端代理桥"作为首选：只有在将来需要"UI 必须调用非插件能力"时才需要它。

## 5. 待办：四个高风险面（需人工）

```bash
export DEEPTUTOR_HOME="$PWD/.phase0-home"
export DEEPTUTOR_DESKTOP_WORKDIR="$PWD"
export DEEPTUTOR_DESKTOP_PYTHON="$PWD/.venv/bin/python"
./desktop/src-tauri/target/debug/deeptutor-desktop
```

| 面 | 判定 |
| --- | --- |
| PDF 阅读器 | 打开大 PDF、翻页、缩放、批注 |
| EPUB | 打开电子书、目录跳转、翻页 |
| 拖拽上传 | 拖文件进窗口有 HTML5 drop 响应（验证 `dragDropEnabled: false` 生效） |
| 导出下载 | 导出的 PDF/DOCX 落到"下载"目录 |

## 6. 发现的问题

1. 🐞 **陈旧状态文件导致误判**（已修复）：外壳启动时若读到上一轮遗留的 `runtime.json`（`status=stopped`），会在 launcher 真正就绪前报"启动失败"。修复：spawn 前删除状态文件 + 只接受 `pid` 等于本外壳子进程的状态 + 失败时 `stop()` 收摊。
2. ⚠️ **冷启 71s** 主要花在首次前端生产构建（热启只要 3s）。这说明"胖包直发"必须把前端构建产物预置进运行时包，否则首启体验不可接受——与已拍板的包体策略一致，Phase 2 要把这条写进打包脚本。
3. ⚠️ **应用命令不可远程调用**（设计约束，非缺陷）：见 §4。
4. ℹ️ `tests/runtime` 一次全量运行中 `test_api_import_memory_boundary` 偶发失败（子进程 stdout 为空），随后 3 次复跑全绿；与本次改动无关，已记录。

## 7. Phase 0 退出标准评估

> macOS 上双击（或 `tauri dev`）一个未签名开发版，能在浏览器之外打开完整 DeepTutor，且四个高风险面全部通过或已有明确替代方案。

**基本达成**：浏览器之外打开完整 DeepTutor 已完成并自动化验证；远程 IPC 的可用性与边界已查清并给出实现规则。唯一未完成项是四个 WebView 高风险面的人工点检（V4–V7），需要人在窗口里操作。
