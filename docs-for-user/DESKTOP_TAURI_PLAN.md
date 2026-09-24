# DeepTutor 桌面化（Tauri v2）方案与实施计划

> 目标：给 DeepTutor 一个真正的桌面应用外壳，同时**不改变 Web 模式与 CLI 模式的任何既有行为**，并让版本更新、打包、发布变成可自动化的一条流水线。

## 0. 结论摘要

1. **不要把前端重写进 Rust，也不要走 Tauri 的静态资源模式。** DeepTutor 的前端是 Next.js 16 的 standalone server，服务端承担同源反代（`/api/*`、`/ws/*` → FastAPI）、鉴权重定向、两个 route handler（知识库上传）与 `next.config` 重定向。用 `output: "export"` 静态化会直接破坏 Web 模式。
2. **Tauri 只当"外壳 + 原生能力层 + 生命周期管理者"，业务进程仍由现有 Python launcher 监督。** 窗口直接加载 `http://127.0.0.1:<frontend_port>`，前端零改动即可跑起来，同源代理链路天然成立。
3. **运行期依赖打包成"运行时包（runtime pack）"，而不是塞进 `.app`。** 外壳（约 20MB，签名公证，走 Tauri updater 更新）与运行时包（Python + 依赖 wheel + Next standalone + Node，放在 app-data，独立版本、独立更新、可回滚）解耦。这是"方便版本更新打包"的关键。
4. **Python 侧只做"纯加法"改动**：`deeptutor start` 新增 `--runtime-info` / `--auto-ports` / `--parent-pid` 三个可选参数，默认行为保持不变；桌面模式靠环境变量 `DEEPTUTOR_DESKTOP_SHELL=1` 显式开启。
5. 分 5 阶段推进：Phase 0 技术验证（1 周）→ Phase 1 外壳 MVP（2–3 周）→ Phase 2 打包与更新（2–3 周）→ Phase 3 桌面体验补全（2–3 周）→ Phase 4 优化与平台扩展（可选，2 周）。

### 0.1 已拍板决策（2026-09-24）

| # | 议题 | 决定 | 对方案的影响 |
| --- | --- | --- | --- |
| 1 | 平台优先级 | macOS + Windows 先 GA，Linux 后置 | v1 CI 矩阵 = macOS(arm64 / x64) + Windows(x64)，共三个包；Linux 归入 v1.x |
| 2 | 包体策略 | 接受首次约 300–600MB 的离线安装包为默认发行形态 | 运行时包随安装载荷直发（放在 `.app` 之外），"首启联网下载"降级为兜底路径 |
| 3 | 是否彻底摆脱 Node | **否** | 删除 Phase 4 的静态导出 Spike；Node 固定随运行时包分发 |
| 4 | API Key 密钥库迁移 | **不算 v1 范围** | 移出 Phase 3，进 backlog；v1 保持现有 `settings/*.json` 存储 |
| 5 | 发布通道 | **仅 stable**，不做 beta/灰度 | 更新清单只维护一个通道，Phase 2 少一层通道逻辑与测试 |

## 1. 现状盘点（基于代码的事实）

| 维度 | 现状 | 证据位置 |
| --- | --- | --- |
| 桌面形态 | **不存在桌面壳**。全仓库没有 Electron/Tauri 代码，`rg -i "electron\|tauri"` 仅命中 `LICENSE`、`web/package-lock.json` 与无关词 | 仓库检索 |
| 启动方式 | macOS 双击 `start_deeptutor.command`，Windows 走 `scripts/start_backend.bat` + `start_frontend.bat`，最终都是终端 + 浏览器 | `start_deeptutor.command` |
| 进程模型 | 一个 launcher 拉起两个子进程：uvicorn（`deeptutor.api.main:app`，默认 8001）与 Next standalone server（`node server.js`，默认 3782），再用浏览器打开 3782 | `deeptutor/runtime/launcher.py:1257`、`services/config/runtime_settings.py:22` |
| 进程监督 | 端口冲突交互式解决；非 TTY 直接报错退出；`_no_window_kwargs()` 已处理 Windows 无控制台窗口；`_kill_port_listeners` 已支持杀占用进程 | `launcher.py:516`、`launcher.py:207`、`launcher.py:489` |
| 就绪信号 | 后端有 `/health/live`、`/health/ready`；`--detach` 模式已写 `data/user/runtime/launcher.json`（含 `status` / `frontend_url` / `backend_port` / `frontend_port`） | `api/main.py:751`、`launcher.py:1142` |
| 前端形态 | Next.js 16 App Router，`output: "standalone"`；`web/proxy.ts` 是中间件，负责把 `/api/*`、`/ws/*` 反代到后端、鉴权 cookie 门禁、Codex OAuth 回调重写；另有 2 个 route handler | `web/next.config.js`、`web/proxy.ts`、`web/app/api/**` |
| 前端分发 | 构建产物进入 `deeptutor_web` 包（`prepare_web_package.py`），运行时复制到 `data/user/runtime/web` 并替换 `__NEXT_PUBLIC_*_PLACEHOLDER__` | `scripts/prepare_web_package.py`、`launcher.py:603` |
| 运行期依赖 | **要求用户机器上有 Node.js 20+ 与 Python**（packaged 模式只在 PATH 找 `node`） | `launcher.py:831` |
| 更新机制 | `detect_installation()` 区分 `docker/source/pypi/unknown`；`pypi` 模式经 `update_worker.py` 执行 `pip install -U deeptutor`，再由 `_handoff_pending_update` 原地重启 | `services/app_update.py:178`、`runtime/update_worker.py` |
| 版本来源 | `deeptutor/__version__.py`（当前 `1.6.10`）是唯一真源，CI 校验 tag 与之一致 | `deeptutor/__version__.py`、`tests/test_release_workflow_guards.py` |
| 发布流水线 | 已有 `pypi-release.yml`、`docker-release.yml`、`tests.yml` | `.github/workflows/` |
| 不可回归的测试 | `tests/runtime/test_launcher.py`、`tests/runtime/test_macos_command_launcher.py`、`tests/runtime/test_update_worker.py`、`tests/services/test_app_update.py`、`tests/test_packaging_metadata.py` | `tests/` |

**一句话**：现在的"桌面版"是"终端 + 浏览器 + 两个后台进程"。Tauri 要补的是**外壳、原生能力、生命周期与分发**，而不是重写应用。

## 2. 需求拆解（可验收定义）

### 2.1 "真正的桌面体验"

不是"无边框浏览器"，而是下面这些可逐条验收的能力：

| # | 能力 | 验收标准 |
| --- | --- | --- |
| D1 | 独立应用 | 安装后 Dock/开始菜单里有 DeepTutor 图标，**不出现任何终端窗口** |
| D2 | 原生窗口 | 记住尺寸/位置/全屏；⌘W / ⌘Q / ⌘, 与菜单栏行为符合平台习惯；编辑菜单的复制/粘贴/全选可用 |
| D3 | 原生生命周期 | 关闭窗口按设置"隐藏到托盘"或"退出"；退出后**没有残留 python/node 进程**；重复启动只聚焦已有窗口 |
| D4 | 原生集成 | 系统通知（长任务完成）、原生文件选择框、在访达/资源管理器中显示、外部链接交给系统浏览器、下载落到"下载"目录 |
| D5 | 系统级入口 | `deeptutor://` 深链、文件关联（PDF/EPUB/MD 用 DeepTutor 打开）、可选开机自启、托盘菜单 |
| D6 | 零依赖安装 | 用户机器上**不需要**预装 Python/Node/npm |
| D7 | 更新体验 | 应用内"检查更新 → 下载 → 重启应用"；区分"外壳更新"和"运行时更新"；失败可回滚 |
| D8 | 可诊断 | 菜单里"打开日志目录"；启动失败给出原生错误对话框 + 明确原因（端口占用 / 运行时包缺失 / 首次解包失败） |

### 2.2 "不影响 Web 和 CLI 模式"

定义成一组**不可破坏的不变量**（见 §6），并配套回归测试。

### 2.3 "方便版本更新打包"

- 一次 `__version__.py` 变更 → 自动同步到 `tauri.conf.json` / `Cargo.toml` / 运行时包清单 / 更新清单；
- CI 一条流水线矩阵产出 macOS(arm64+x64) / Windows(x64) 安装包 + 签名更新产物（Linux 在 v1.x 加入）；
- 更新产物大小可控：外壳更新约 10–20MB，运行时更新按需；
- 用户数据（知识库、会话、设置）跨版本、跨外壳/运行时更新都不丢。

## 3. 关键架构决策

### 决策 1：窗口里装什么？

| 方案 | 做法 | 优点 | 代价 | 结论 |
| --- | --- | --- | --- | --- |
| **A. 指向本地 Next 服务**（推荐） | Tauri 窗口加载 `http://127.0.0.1:<frontend_port>` | 前端 0 改动；同源反代/中间件/route handler 全部保留；Web 与桌面共用同一份构建产物 | 仍需 Node 运行时（由运行时包提供，用户无感） | ✅ Phase 1 采用 |
| B. 静态导出 + 自定义协议 | `output: "export"`，窗口加载 `tauri://localhost` | 彻底摆脱 Node | 必须重做反代（改 CORS + cookie 语义）、中间件改客户端守卫、route handler 搬到后端、重定向改客户端；Web 构建配置要分叉 | ⏸ Phase 4 再评估 |
| C. Node SEA / 单文件前端 | 把 Next standalone 打成单可执行文件 | 少一个进程 | 体积大、构建脆弱、收益低于 B | ❌ 不采纳 |

> 关键理由：`web/proxy.ts` 承担的不只是反代，还有**鉴权 cookie 门禁**与 **Codex OAuth 回调重写**。若窗口加载自定义协议，这些请求会变成跨源/第三方，cookie 与鉴权链路需要整套重设计。方案 A 一次性规避这些风险。

### 决策 2：运行期依赖怎么分发？

| 方案 | 做法 | 优点 | 代价 | 结论 |
| --- | --- | --- | --- | --- |
| P1. 复用用户环境 | 沿用现状，要求用户装 Python + Node | 0 包体、0 CI 成本 | 不满足 D6，称不上桌面产品 | ❌ 仅作开发模式 |
| **P2. 内嵌 CPython + 真实 wheel**（推荐） | python-build-standalone 发行版 + `uv` 从锁定的 wheelhouse 安装出 venv，连同 Next standalone 一起打成运行时包 | 不做冻结，原生扩展（faiss/PyMuPDF/numpy/tiktoken）保持原样；标准 venv，升级 = 换包；CI 可复现 | 包体 300–600MB；需建锁文件与打包脚本 | ✅ 采用 |
| P3. PyInstaller 冻结 | `--collect-all llama_index` 等 | 单目录可执行 | llama-index/向量库/MCP 大量动态导入与资源文件，冻结脆弱、排查成本高、每次依赖升级都是风险 | ❌ 不推荐（可作 Phase 4 备选） |

配套策略（同时解决体积问题）：

- **核心 + 按需扩展**：运行时包只装核心依赖。重扩展（`parse-docling`、`rag-rerank`、`math-animator`、`graphrag`、`partners`、`codebuddy`）在设置页按需下载安装——仓库已有 `scripts/install_extras.py` 的现成模式可复用。
- **Node 固定随包分发**（已拍板不做静态导出）：把 Node 20 LTS 二进制放进运行时包，用户机器上永远不需要 Node；前端继续用 Next standalone 的完整能力。

### 决策 3：谁来监督进程？

**复用 Python launcher，Rust 只做父进程。** 理由：

- 端口冲突处理、`data/user/settings/*.json` 持久化、打包前端占位符替换、更新交接与重启、`memory_probe` 的 supervisor PID——全都在 `deeptutor/runtime/launcher.py`，且有测试覆盖；
- 在 Rust 里重写一遍等于维护第二套启动语义，是 Web/CLI 不一致的头号来源；
- Rust 侧只负责：启动/停止 launcher、就绪握手、崩溃重启、窗口与原生能力。

### 决策 4：更新分两个平面

| 平面 | 内容 | 通道 | 典型体积 |
| --- | --- | --- | --- |
| Plane A：外壳 | Tauri 应用本体（Rust + 图标 + 原生逻辑） | `tauri-plugin-updater` + GitHub Releases 的 `latest.json`（与现有版本检查同源） | 10–20MB |
| Plane B：运行时包 | CPython + venv + `deeptutor_web` 静态产物 + Node | 自建 `runtime-packs.json` 清单，外壳下载 → 校验 → 解包 → 冒烟 → 原子切换 | 50–400MB |

好处：只改原生交互时用户只下 20MB；只升级依赖或前端时外壳不动；任一平面失败都能回退。

## 4. 目标架构

```
              ┌──────────────────────────────────────────────┐
              │  DeepTutor.app / DeepTutor.exe（Tauri v2）    │  ← 已签名，Tauri updater 更新
              │                                              │
              │  • 窗口（WebView 指向 127.0.0.1:3782）        │
              │  • Splash / 原生错误对话框 / 菜单 / 托盘       │
              │  • 单实例 / 深链 / 文件关联 / 通知 / 下载      │
              │  • Supervisor：启动并守护 Python launcher      │
              │  • Runtime Pack Manager：定位/校验/切换       │
              └───────────────┬──────────────────────────────┘
                              │ spawn + runtime.json 握手
                              ▼
              ┌──────────────────────────────────────────────┐
              │  deeptutor start --runtime-info ...（既有）   │  ← CLI/Web 共用同一份代码
              │  deeptutor/runtime/launcher.py               │
              └───────┬──────────────────────────┬───────────┘
                      │                          │
            uvicorn :8001                node server.js :3782
            (FastAPI + WS + RAG)         (Next standalone + 同源反代)
                      ▲                          ▲
                      └───────────┬──────────────┘
                                  │
                    运行时包：<app-data>/DeepTutor/runtimes/<pack>/
                    （python + venv + deeptutor_web + node）
```

### 目录布局

```
macOS:   ~/Library/Application Support/DeepTutor/
Windows: %LOCALAPPDATA%\DeepTutor\
Linux:   ~/.local/share/DeepTutor/
├── data/                      # 既有 runtime home（settings / kb / sessions / outputs）
├── desktop/
│   ├── runtime.json           # 新增：就绪握手（status / ports / frontend_url / token）
│   ├── state.json             # 外壳状态：当前 pack、窗口、更新通道
│   └── logs/{shell,launcher,backend,frontend}.log
└── runtimes/
    ├── 1.6.10-pack1/          # 当前运行时包
    ├── 1.6.11-pack1/          # 新包（切换成功后成为 active）
    └── 1.6.10-pack1.bak/      # 回滚用（保留最近 1 个）
```

要点：`data/` 就是现有 DeepTutor 运行时目录，CLI 用户也可以用 `DEEPTUTOR_HOME` 指过来共享同一份数据。

## 5. 组件设计

### 5.1 桌面壳（Rust 侧模块）

```
desktop/
├── src-tauri/
│   ├── Cargo.toml
│   ├── tauri.conf.json
│   ├── capabilities/
│   │   ├── main.json          # 窗口/菜单/托盘/通知/对话框/opener
│   │   └── remote-web.json    # 远程域 IPC 授权（见 5.4）
│   ├── icons/                 # 复用 assets/ 品牌图，脚本生成 .icns/.ico
│   └── src/
│       ├── main.rs
│       ├── supervisor.rs      # 拉起 launcher、握手、崩溃重启、优雅退出
│       ├── runtime_pack.rs    # 定位/校验/解包/冒烟/原子切换/回滚
│       ├── update.rs          # 外壳更新 + 运行时包更新 + 重启编排
│       ├── native.rs          # 菜单、托盘、深链、文件关联、下载回调
│       └── state.rs           # 窗口状态、最近会话、通道配置
├── pack/
│   ├── runtime.lock.json      # python 版本 + wheel 清单与哈希 + node 版本
│   └── build_pack.py          # 产出 runtime-<ver>-<os>-<arch>.tar.zst + sha256
└── scripts/
    ├── sync_version.py        # __version__.py → tauri.conf.json / Cargo.toml
    └── smoke_pack.sh          # 包冒烟：import deeptutor / node --version / server.js 起停
```

`tauri.conf.json` 关键项（示意）：

```jsonc
{
  "identifier": "com.deeptutor.desktop",      // 一旦发布不要改，否则 WebView 存储会丢
  "productName": "DeepTutor",
  "version": "1.6.10",                        // 由 sync_version.py 从 __version__.py 生成
  "app": {
    "withGlobalTauri": true,                  // 让本地页面拿到 window.__TAURI__
    "windows": [{
      "label": "main",
      "url": "splash",                        // 先 splash，握手成功后导航到 frontend_url
      "title": "DeepTutor",
      "width": 1280, "height": 860,
      "minWidth": 960, "minHeight": 640,
      "dragDropEnabled": false                // 关键：否则 WebView 收不到 HTML5 拖拽事件
    }]
  },
  "bundle": {
    "targets": ["dmg", "nsis", "msi"],        // Linux 的 deb/appimage 在 v1.x 跟进阶段加入
    "macOS": { "minimumSystemVersion": "11.0" },
    "windows": { "nsis": { "installMode": "currentUser" } }
  },
  "plugins": {
    "updater": {
      "endpoints": ["https://github.com/HKUDS/DeepTutor/releases/latest/download/latest.json"],
      "pubkey": "<minisign 公钥>"
    }
  }
}
```

### 5.2 就绪握手与新增 CLI 契约（**全部为可选参数，默认不变**）

对 `deeptutor start` 只加三个 flag 与一个环境变量：

| 新增 | 作用 | 默认 |
| --- | --- | --- |
| `--runtime-info PATH` | 原子写出机器可读状态：`{"schema_version":1,"status":"starting\|ready\|stopped","frontend_url":...,"backend_port":...,"frontend_port":...,"pid":...,"token":...}` | 关闭 |
| `--auto-ports` | 端口占用时不交互，自动挑空闲端口并持久化到 `system.json`（桌面没有 stdin 可答） | 关闭（保持交互/非 TTY 报错的旧行为） |
| `--parent-pid PID` | 父进程消失则自动收摊，避免外壳被强杀后留下孤儿 python/node | 关闭 |
| `DEEPTUTOR_DESKTOP_SHELL=1` | 桌面模式标记：隐含上面三项；禁用 pip 自更新交接（见 5.5）；日志走文件而非终端 | 未设置 |

> `--detach` 模式已经有 `data/user/runtime/launcher.json` 这份"就绪文件"的雏形（`launcher.py:1142`）。实现时把它抽成同一个写文件函数，桌面模式复用，避免出现第二套协议。

握手时序：

```
Rust                      Python launcher            uvicorn / node
 │  spawn(..., --runtime-info)  │                        │
 │──────────────────────────────▶│  写 status=starting    │
 │                               │─────── spawn ─────────▶│
 │  轮询 runtime.json /          │  /health/ready +       │
 │  http://127.0.0.1:3782/       │  前端 HTTP 就绪        │
 │◀──────── status=ready ────────│                        │
 │  导航主窗口 → frontend_url，关闭 splash                │
 │  ⌘Q → SIGTERM launcher → 子进程树清理 → 等 5s → KILL    │
```

### 5.3 运行时包（Runtime Pack）

构建（在 CI 里，不在用户机器上）：

1. 下载并校验 python-build-standalone（按 OS/arch 固定版本 + 哈希）。
2. `uv venv` + `uv pip install --no-index --find-links wheelhouse -r runtime.lock.json`：所有依赖从预下载的 wheelhouse 安装，**零编译、结果可复现**。
3. 放入 `deeptutor_web`（由 `npm ci && npm run build && python scripts/prepare_web_package.py` 产出）与 Node 20 LTS。
4. `smoke_pack.sh`：`python -c "import deeptutor"`、`node --version`、启动 `server.js` 并 `curl /`、跑 `tests/runtime` 子集。
5. 产出 `runtime-<app_version>-pack<n>-<os>-<arch>.tar.zst` + `sha256`，并写入 `runtime-packs.json`。

发行形态（已拍板：离线胖包为默认）：

- **默认（胖包直发）**：安装载荷 = `.app` / 安装器 + `runtime-<ver>-<os>-<arch>.tar.zst`（两者并列放在 DMG / 安装目录里，**不放进 `.app` 内部**）。首启时外壳从安装介质本地解包到 `runtimes/<pack>/`，全程离线。
- **兜底（首启下载）**：安装载荷未携带运行时包，或需要更高版本包时才联网下载，失败可重试。
- 两者共用同一套"校验 → 解包 → 冒烟 → 原子切换"代码，差别只是 source 为 `local` 或 `remote`。

安装（用户机器上，外壳执行）：

```
下载 → sha256 校验（可选 minisign）→ 解包到 runtimes/<pack>.tmp/ → 冒烟
     → 原子 rename → 更新 state.json → 下次启动生效
失败 → 保留旧 pack，删除 tmp，原生对话框报错
```

清单格式（`runtime-packs.json`，与 `latest.json` 一起挂在 Release 上）：

```jsonc
{
  "schema_version": 1,
  "packs": [{
    "pack_id": "1.6.11-pack1",
    "app_version_range": ">=1.6.0,<1.7.0",
    "python": "3.12.7",
    "node": "20.18.0",
    "platforms": {
      "darwin-aarch64": { "url": "...", "size": 0, "sha256": "..." },
      "windows-x86_64": { "url": "...", "size": 0, "sha256": "..." }
    },
    "requires_shell": ">=1.2.0"
  }]
}
```

### 5.4 原生能力与前端桥接

推荐做法：**Tauri IPC over remote URL**——页面本身来自 `http://127.0.0.1:<port>`，用 capability 的 `remote.urls` 把 IPC 精确授权给这个源（配合 `app.withGlobalTauri`）。这样前端可以直接调用 `@tauri-apps/api`，而在纯浏览器/CLI 模式下同一套代码自动降级。

| 能力 | 实现 | 触发方 | 阶段 |
| --- | --- | --- | --- |
| 系统通知（回合/长任务完成） | `tauri-plugin-notification` | 前端在 `document.hidden` 时调用 | P3 |
| 托盘 + 菜单 | Rust tray/menu | 显示/隐藏、重启服务、检查更新、打开日志、退出 | P1 |
| 原生文件选择 | `tauri-plugin-dialog` | 附件、知识库上传 | P3 |
| 在访达/资源管理器中显示 | `tauri-plugin-opener` | 导出结果卡片 | P3 |
| 外部链接 | `tauri-plugin-opener` + 拦截 `target=_blank` | 引用/文档链接 | P1 |
| 下载落盘 | WebView `on_download` | 导出 PDF/DOCX | P1 |
| 单实例 | `tauri-plugin-single-instance` | 二次启动聚焦并转发参数 | P1 |
| 深链 `deeptutor://` | `tauri-plugin-deep-link` | 会话/知识库直达 | P3 |
| 文件关联 | `bundle.fileAssociations` | PDF/EPUB/MD 用 DeepTutor 打开 | P3 |
| 窗口状态记忆 | `tauri-plugin-window-state` | 尺寸/位置/全屏 | P1 |
| 剪贴板兜底 | `tauri-plugin-clipboard-manager` | WKWebView 下 `navigator.clipboard` 异常时 | P3 |
| 密钥库 | `tauri-plugin-stronghold` 或 OS keyring | API Key 从明文 JSON 迁到 Keychain/Credential Manager，启动时经环境变量注入（复用既有 `export_runtime_settings_to_env`）。**已拍板不做 v1**，进 backlog | 暂不做 |
| 自动更新 | `tauri-plugin-updater` | 菜单/后台检查 + 重启应用 | P2 |
| 开机自启（可选） | `tauri-plugin-autostart` | 设置项 | P3 |

✅ **Phase 0 已验证（2026-09-24，macOS WKWebView）**：

| 结论 | 证据 |
| --- | --- |
| `remote.urls` 的 `http://127.0.0.1:*` / `http://localhost:*` 通配**生效** | 从 `http://localhost:3783` 调用 core 插件命令 `plugin:app\|version` 返回 `1.6.10`（显式端口与通配两种写法都放行）→ **端口可继续浮动，不需要固定端口池** |
| **Tauri 应用自定义命令（`generate_handler!`）无法授权给远程源** | 同一页面调用 `desktop_probe` 返回 `not allowed. Plugin not found`（应用命令不带 ACL 条目） |

由此确定的桥接规则：**UI 需要调用的外壳能力一律做成自定义 Tauri 插件（带 permission set）**，即新建 `tauri-plugin-deeptutor` 承载"桌面状态查询 / 重启服务 / 运行时包信息"等自有命令；OS 级能力（通知、对话框、opener、updater、剪贴板、窗口）本身就是插件，远程直接可用，无需后端改造。

"后端代理桥"（`/api/desktop/*`）降级为备选：只有当将来必须让 UI 调用某个**无法插件化**的能力时才启用。

### 5.5 版本、更新与打包

**版本唯一真源不变**：`deeptutor/__version__.py`。新增 `desktop/scripts/sync_version.py`，构建前把它写进 `tauri.conf.json`、`Cargo.toml`、`runtime-packs.json`，并把"三者一致"加进 `tests/test_release_workflow_guards.py` 的既有守卫。

新增 `.github/workflows/desktop-release.yml`（tag 触发，与 `pypi-release.yml` 并行）：

```
validate-tag（复用现有守卫：tag == __version__）
   ├── build-web        : npm ci && npm run build && prepare_web_package.py（三平台复用同一份产物）
   ├── build-packs      : matrix [macos-14(arm64), macos-13(x64), windows-2022]
   │                      → build_pack.py → 冒烟 → 上传 runtime-*.tar.zst
   ├── build-shells     : matrix 同上 → sync_version.py → tauri build
   │                      → macOS: Developer ID 签名 + notarytool 公证 + staple
   │                      → Windows: 代码签名
   │                      → 上传 dmg/nsis/msi
   └── publish-update   : 生成 latest.json（Tauri updater 签名）+ runtime-packs.json
                          → 附加到 GitHub Release
```

v1 平台矩阵已定为 **macOS(arm64 / x64) + Windows(x64)**（已拍板）。Linux 的 `ubuntu-22.04` 构建、`deb`/`rpm`/AppImage 目标在 v1.x 跟进阶段加入，届时复用同一份 `runtime-packs.json` 结构（多一个平台键即可）。

发布通道：**仅 `stable` 一条**（已拍板），指向 `releases/latest` 的 `latest.json`。预发布 tag 只产出构建产物，不更新 `latest.json`，因此不会推送给普通用户。

### 5.6 签名、公证与安全

- **macOS**：Developer ID Application 证书 + `notarytool` 公证 + `stapler`。**不要开启 App Sandbox**（沙箱下无法 spawn python/node，等于放弃这套架构）；因此不上 Mac App Store，走 DMG 直发——这与"自动化更新"的目标一致。
- **不要往 `.app` 里写文件**。macOS 下修改已签名包内容会破坏签名并可能让系统拒绝启动，所以 pip 自更新在桌面模式必须被禁用（见 §7），运行时包一律放 app-data。
- 运行时包从 tar 解包后要**清除 `com.apple.quarantine`**，否则首次执行可能被 Gatekeeper 拦下。
- **Windows**：Authenticode 签名（否则 SmartScreen 告警）；NSIS 附带 WebView2 bootstrapper，覆盖未预装 WebView2 的机器。
- **Linux（v1.x 跟进）**：Tauri 依赖 `webkit2gtk-4.1`；AppImage 需要宿主机具备 WebKitGTK，也可用 `deb`/`rpm` 声明依赖。

### 5.7 WebView 兼容性风险清单（Phase 0 逐项打勾）

桌面模式把 Chrome 换成 WKWebView / WebView2 / WebKitGTK，下面这些必须实测：

| 面 | 风险 | 应对 |
| --- | --- | --- |
| PDF 阅读器（`pdfjs-dist`） | WKWebView 下 worker/canvas 行为差异 | 用例：打开大 PDF、翻页、缩放、批注 |
| EPUB（`epubjs`）与 DOCX 预览 | 本地文件/blob URL 与 iframe 限制 | 用例 + 必要时改 blob 注入方式 |
| 拖拽上传 | Tauri 默认拦截 drop 事件 | 窗口 `dragDropEnabled: false` |
| 导出（jspdf / docx-preview `<a download>`） | WebView 下载默认不落地 | `on_download` → 系统下载目录 + 通知 + 在文件夹中显示 |
| 剪贴板 | WKWebView 对 `navigator.clipboard` 有额外要求 | clipboard 插件兜底（loopback 属安全上下文，通常可用） |
| 语音朗读 / 录音 | 需要授权与用户手势；`speechSynthesis` 可用性不同 | 按 `docs-for-user/VOICE_CONFIGURATION.md` 的路径逐项验证 |
| OAuth 登录（Codex / provider） | 需要系统浏览器 + 回调回到 loopback | 外部链接交给系统浏览器；回调命中本地服务时把窗口拉到前台 |
| 存储持久化 | WebView 数据目录绑定 bundle identifier | 早期固定 `identifier` 并写入文档，不允许改 |
| 代理/网络错误 | WebView 原生错误页体验差 | 后端未就绪时显示原生错误页 + "重试 / 打开日志" |

## 6. 兼容性契约：什么可以改，什么不能改

### 6.1 不变量（任何 PR 都不得破坏）

1. `deeptutor start` / `deeptutor serve` / `deeptutor chat` **在不带新参数的默认路径下行为逐字节不变**（含 `open_browser=True` 默认、端口冲突交互提示、非 TTY 报错退出）。
2. 新增参数必须可选，且只在显式传入（或 `DEEPTUTOR_DESKTOP_SHELL=1`）时生效。
3. 后端 HTTP/WS 契约不变：`/health/*`、`/api/*`、`/ws` 的语义与 payload 不因桌面化而改；桌面专用能力只能以**新增**端点或前端探测形式出现。
4. `data/` 目录结构、`settings/*.json` 结构、PyPI wheel 内容不变（`desktop/` 落在 `deeptutor*` / `deeptutor_cli*` 之外，天然不进 wheel；`MANIFEST.in` 只收 `deeptutor_web`）。
5. 默认 runtime home 语义不变：CLI 仍默认 `cwd`，桌面通过显式 `--home` 使用 app-data。
6. `deeptutor_web` 的构建与占位符替换机制不拆（桌面与 Web 共用）。
7. 既有测试必须继续通过：`tests/runtime/test_launcher.py`、`test_macos_command_launcher.py`、`test_update_worker.py`、`tests/services/test_app_update.py`、`test_packaging_metadata.py`。

### 6.2 允许的"纯加法"

| 文件 | 变更 |
| --- | --- |
| `deeptutor/runtime/launcher.py` | 新增 `--runtime-info` / `--auto-ports` / `--parent-pid` 三条可选分支；抽出 `_write_runtime_state()` 供 detached 与 desktop 复用 |
| `deeptutor_cli/main.py` | `start` 命令新增三个 `typer.Option`（默认 `None`/`False`），原样透传 |
| `deeptutor/services/app_update.py` | `InstallMode` 增加 `"desktop"`；`DEEPTUTOR_DESKTOP_SHELL=1` 时 `automatic_update=False`，`command` 指向外壳 updater，`reason` 说明原因 |
| 前端 | 新增 `web/lib/desktop.ts`（探测 + 原生桥封装，浏览器模式 no-op）；设置页新增"桌面"分区 |
| 其余 | 全部落在 `desktop/` 与 `.github/workflows/desktop-*.yml` |

### 6.3 三种模式对照

| 模式 | 启动 | 前端来源 | 依赖 | 更新方式 | 数据目录 |
| --- | --- | --- | --- | --- | --- |
| CLI | `deeptutor run chat ...` | 无 | 用户 Python 环境 | `pip install -U deeptutor` | `cwd` 或 `DEEPTUTOR_HOME` |
| Web | `deeptutor start`（现状不变） | 用户 Node + `deeptutor_web`（不变） | 用户 Python + Node | 同上 | 同上 |
| Desktop | 双击应用 | 运行时包内的 Node + `deeptutor_web` | 无（自带） | 外壳 updater + 运行时包 | app-data（显式 `--home`） |

## 7. 实施计划

### Phase 0 — 技术验证（1 周，1 人）

目标：把"能不能这么干"变成"已验证"。

> 进展与验证结果记录在 [`desktop/PHASE0_REPORT.md`](../../desktop/PHASE0_REPORT.md)。

- [x] 最小 Tauri v2 工程：窗口在 launcher 就绪后加载本地 UI（菜单/⌘Q 归 Phase 1）。
- [x] 验证 remote URL IPC 授权 + `withGlobalTauri`：core 插件命令从 loopback 页面调用成功；顺带查清"应用命令不可远程调用"这一硬约束。
- [x] 验证 capability 对可变端口的匹配：`http://127.0.0.1:*` 通配生效，**不需要固定端口池**。
- [x] 验证 Rust spawn python launcher、`--runtime-info` 握手、退出/强杀后无残留进程（热启动 3s，强杀父进程 3s 内收敛）。
- [ ] 冒烟 §5.7 中风险最高的 4 项：PDF、EPUB、拖拽上传、导出下载 —— **需人工点检**。
- [x] 度量运行时包体积与启动耗时：冷启 71s（含首次前端构建）、热启 3s；运行时包粗估 600–650MB 未压缩。
- [x] 产出：`desktop/` 骨架 + `desktop/PHASE0_REPORT.md` + 两个自动验证脚本。

**退出标准**：macOS 上双击一个未签名开发版，能在浏览器之外打开完整 DeepTutor，且 4 项高风险面全部通过或已有明确替代方案。

当前状态：**基本达成**，只剩 4 项 WebView 面的人工点检。

### Phase 1 — 桌面外壳 MVP（2–3 周，1–2 人）

- [x] **`tauri-plugin-deeptutor`（先行项）**：外壳命令（`desktop_status` / `restart_service`）做成带 permission set 的插件，capability 授予 `deeptutor:default`；运行时已验证 loopback 页面可直接调用。
- [x] `supervisor.rs`：spawn / 握手 / 崩溃重启（2s/4s/8s 退避，最多 3 次）/ 优雅退出（SIGTERM → 5s → KILL 进程组）/ 父进程死亡自清理；`generation` 机制保证手动重启不被误判成崩溃。
- [x] splash 窗口 + 原生错误对话框（失败时先收摊子进程，再写日志、splash 红字与系统弹窗）。
- [x] 原生菜单（DeepTutor/编辑/视图/窗口/帮助，⌘, → 设置页，⌘R 重载）、窗口状态记忆、文件日志 + "打开日志目录"；另附托盘（显示/重启/日志/退出 + 左键唤回）。
- [x] 单实例 + 二次启动聚焦。
- [x] Launcher 三个新参数的实现 + 单元测试（含"不带参数行为不变"的回归断言）——Phase 0 提前完成。
- [x] `detect_installation()` 增加 `desktop` 模式：`automatic_update=False`，设置页走既有 "Managed by your installation" 分支展示原因与命令（与 docker/source 同路径；文案本地化留待 Phase 3）。
- [x] CI 增加 `desktop-ci.yml`：`desktop/**` 变更时跑 fmt / clippy -D warnings / 单测 / 构建 / `--self-check` 冒烟（Windows 调试包在 Phase 2 并入矩阵）。

> 进展与运行时证据见 [`desktop/PHASE1_REPORT.md`](../../desktop/PHASE1_REPORT.md)。

**退出标准**：安装 → 点开 → 5 秒内出现窗口；⌘Q 后无残留 uvicorn/server.js 进程；异常退出能被外壳兜住并给出可读错误。

### Phase 2 — 运行时包、签名与更新（2–3 周，1–2 人）

- [ ] `runtime.lock.json` + `build_pack.py`：python-build-standalone + wheelhouse + Node + `deeptutor_web`，构建 macOS(arm64 / x64) 与 Windows(x64) 三个包。
- [ ] 胖包直发：安装载荷携带运行时包，首启从本地解包（离线可用）；联网下载作为兜底路径。
- [ ] `runtime_pack.rs`：定位/校验/解包/冒烟/原子切换/回滚，`local` 与 `remote` 两种 source 共用同一路径。
- [ ] `tauri-plugin-updater`：单一 stable 通道的 `latest.json` 与 `runtime-packs.json`。
- [ ] 更新 UX：检查更新 → 进度 → "重启以应用" → 复用既有 `VersionCheckService` 拿到的 Release 说明展示更新日志。
- [ ] macOS 签名+公证+stapling、Windows 签名；产物齐全（dmg / nsis / msi）。
- [ ] `desktop-release.yml` 全流程跑通，`sync_version.py` 接入发布守卫测试。

**退出标准**：从 `1.6.10` 更新到 `1.6.11`：外壳与运行时各自独立更新成功、用户数据完整、中断下载后重启能自愈、投毒一个坏包能被外壳拒绝并回滚；断网环境下首次安装（胖包）可用。

### Phase 3 — 桌面体验补全（2–3 周）

- [ ] 托盘（显示/隐藏、重启服务、检查更新、退出）+ 关闭窗口到托盘（可配置）+ 长任务继续运行。
- [ ] 系统通知（回合完成、长任务完成）+ 点击回到对应会话。
- [ ] 原生文件对话框（附件、知识库上传）、导出后"在文件夹中显示"。
- [ ] `deeptutor://` 深链与文件关联（PDF/EPUB/MD），拖到 Dock 图标也能打开。
- [ ] 首启向导：语言、模型/密钥、数据目录、可选扩展（复用 `scripts/install_extras.py` 模式做按需下载）。
- [ ] 前端 `isDesktop()` 探测与降级：浏览器模式下这些入口隐藏或退化为 Web 行为。

**退出标准**：窗口隐藏后完成一次 `deep_research`，能在系统通知里看到结果并点击回到该会话；三种模式的功能差异仅限"原生增强项"。

> 已拍板移出 v1：API Key 迁移到 OS 密钥库（进 backlog，v1 保持现有 `settings/*.json` 存储）。

### Phase 4 — 优化与平台扩展（可选，2 周）

- [ ] Spike B：运行时包增量更新（只换变更的 wheel / 前端产物），把常见更新从 300MB 降到 10–30MB。
- [ ] Linux 接入：`deb`/`rpm` 优先，AppImage 标注 WebKitGTK 宿主依赖；加入 CI 矩阵与 `runtime-packs.json` 平台键。
- [ ] Windows/Linux 体验打磨（任务栏、通知、字体、WebView2 兜底）。
- [ ] 启动性能：splash 到首屏的耗时埋点与优化。

## 8. 测试与 CI

| 层 | 内容 |
| --- | --- |
| Python 单测 | 新参数：`--runtime-info` 的写入内容/原子性/`schema_version`；`--auto-ports` 在占用时自动换端口并持久化；`--parent-pid` 父进程消失后收摊；**默认路径行为快照不变** |
| Python 回归 | 复用 `tests/runtime/test_launcher.py`、`test_macos_command_launcher.py`、`tests/services/test_app_update.py`；新增 `desktop` 安装模式判定用例 |
| Rust 单测 | 包校验与回滚、状态机（starting→ready→stopping）、退避重启、进程组清理 |
| 端到端（每平台） | 安装包安装 → **断网首启（验证胖包本地解包）** → 发一轮对话 → 上传 PDF → 导出 → 更新 → 卸载后残留检查 |
| 前端 e2e | 现有 Playwright 用例在桌面窗口内跑关键路径；新增 `isDesktop` 降级用例 |
| 发布守卫 | tag == `__version__.py` == `tauri.conf.json` == `Cargo.toml`；更新清单里的 sha256 与产物一致 |

## 9. 风险登记表

| # | 风险 | 影响 | 概率 | 对策 |
| --- | --- | --- | --- | --- |
| R1 | 运行时包过大（核心依赖含 faiss/PyMuPDF/llama-index） | 安装/更新体验差 | 高 | 已拍板胖包直发（下载成本一次性）；体积靠"核心 + 按需扩展"控制；Phase 4 增量更新降低后续更新成本 |
| R2 | WKWebView 与 PDF/EPUB/音频/剪贴板兼容 | 关键功能不可用 | 中 | Phase 0 逐项冒烟，早暴露早改 |
| R3 | macOS 公证与 Windows 代码签名配置 | 发布卡住 | 中 | Phase 2 提前一周做签名流水线演练；证书/密钥走 CI secret |
| R4 | 用户强杀外壳导致孤儿进程 | 端口占用、内存泄漏 | 中 | `--parent-pid` + 进程组清理 + 启停自检用例 |
| R5 | Tauri remote IPC 授权与可变端口不兼容 | 前端桥接方案受阻 | **已关闭** | Phase 0 实测：通配端口 grant 生效；真正的约束是"应用命令不可远程调用"→ 自有命令改做自定义插件（§5.4） |
| R6 | 桌面模式误触发 pip 自更新，破坏签名包 | 应用无法启动 | 中 | `detect_installation()` 新增 `desktop` 模式并在桌面路径硬禁 pip 更新 |
| R7 | 三模式出现行为分叉 | Web/CLI 回归 | 中 | §6.1 不变量清单 + 默认路径快照测试 + 桌面改动集中在 `desktop/` |
| R8 | Linux WebKitGTK/AppImage 依赖 | 覆盖不全 | 低（v1 不含 Linux） | v1 只发 macOS + Windows；Linux 在 v1.x 跟进阶段先做 deb/rpm，AppImage 标注宿主依赖 |

## 10. 排期与人力

| 阶段 | 时长 | 人力 | 里程碑 |
| --- | --- | --- | --- |
| Phase 0 | 1 周 | 1 人 | 可行性报告 + 骨架 |
| Phase 1 | 2–3 周 | 1–2 人 | macOS 开发版可用（无终端） |
| Phase 2 | 2–3 周 | 1–2 人 | 三平台（mac arm64/x64、win x64）安装包 + 双层更新 |
| Phase 3 | 2–3 周 | 1–2 人 | 原生体验补全 |
| Phase 4 | 2 周（可选） | 1 人 | 瘦身/提速 |

v1 GA 范围 = **macOS(arm64 + x64) + Windows(x64)**，离线胖包直发，仅 stable 通道。单工程师串行约 7–9 周到 GA；两人并行（一人外壳+原生化，一人运行时包+CI）约 4–5 周。

## 11. 明确不做（v1）与遗留细节

**已明确不做：**

- 不做前端静态导出 / 不消除 Node 依赖（Phase 4 Spike A 已删除）。
- 不做 API Key 密钥库迁移（backlog）。
- 不做 beta / 灰度发布通道。
- 不做 Linux 发行包（v1.x 跟进）；不做 Mac App Store 版本（沙箱与 spawn 子进程不兼容）。

**仍需在 Phase 0 实测后定，但不阻塞开工：**

1. Tauri capability 对可变端口的匹配 → 决定"固定端口池"还是"后端代理桥"（§5.4）。
2. 关闭窗口的默认行为（隐藏到托盘 / 直接退出）与托盘图标默认可见性——按平台习惯在 Phase 3 定，先做成设置项。
3. 开机自启默认关闭，是否进 v1 设置页（实现成本低，可随 Phase 3 一起做）。
4. `identifier` 最终取值（`com.deeptutor.desktop` 为占位）——一旦发布不可再改，Phase 1 定稿。
