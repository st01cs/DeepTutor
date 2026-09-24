# Phase 3 进展报告

日期：2026-09-24 · 范围：桌面体验补全（托盘与关窗策略、系统通知、原生文件、
`deeptutor://` 深链与文件关联、首启向导、前端 `isDesktop()` 探测与降级）

## 0. 结论

Phase 3 的六项清单全部落地，并且在真实进程上做了端到端验证：托盘 + 关窗策略 +
系统通知 + "点击回到会话"的完整链路、`deeptutor://` 与文件关联的 hand-off 队列、
首启向导（含数据目录搬迁）、以及前端在三种模式下的降级路径。

| 项 | 状态 |
| --- | --- |
| 托盘（显示/隐藏、重启服务、检查更新、打开日志、退出） | ✅ 菜单项齐全，左键切换窗口 |
| 关闭窗口到托盘（可配置）+ 长任务继续运行 | ✅ 菜单栏勾选项 + 设置页开关，落 `desktop/shell.json` |
| 系统通知（回合完成）+ 点击回到对应会话 | ✅ 后台完成才发；点击/ Dock 唤醒后投递路由（机制见 §2.1） |
| 原生文件对话框（附件/知识库、数据目录） | ✅ `pick_files` / `pick_folder`，UI 侧 `read_local_file` 复用现有上传链路 |
| 导出后"在文件夹中显示" | ✅ 下载落到 `~/Downloads`（重名自动改名）+ `reveal_in_folder` |
| `deeptutor://` 深链 + 文件关联（PDF/EPUB/MD）+ 拖到 Dock | ✅ 语法与安全边界见 §2.2；Dock 拖放与"打开方式"共用一条队列 |
| 首启向导（语言、数据目录、桌面行为、模型/密钥交接） | ✅ 向导在 splash 内（app origin），落 `desktop/shell.json` + `bootstrap.json` |
| 可选扩展（按需下载） | ⏸ 见 §4.2：随胖包直发的核心依赖已覆盖，扩展下载留 backlog |
| 前端 `isDesktop()` 探测与降级 | ✅ `lib/desktop-shell.ts` + `features/desktop/*`，浏览器模式全部 no-op |
| 设置页"桌面"分区 | ✅ `/settings/desktop`（浏览器下降级为说明页，理由见 §2.4） |
| 外壳单测 | ✅ 49 passed（Phase 2 为 18）+ 前端 11 条新用例 |
| 窗口尺寸/位置/全屏记忆 | ✅ 修好了一个真 bug（Tauri 2.11 从不调用 window-state 的恢复钩子，见 §3.5），已用预置状态文件实测 |
| macOS 签名/公证、Windows 签名、`tauri-plugin-updater` | ⛔ 与 Phase 2 相同：需要证书与真实 release 资产 |
| WebView 高风险面人工点检（PDF / EPUB / 拖拽 / 导出） | ⏸ 仍需人工（见 §4.1） |

## 1. 交付清单（按文件）

### 1.1 外壳（Rust）

| 文件 | 作用 |
| --- | --- |
| `src-tauri/src/settings.rs` | `desktop/shell.json`（偏好）+ `desktop/bootstrap.json`（数据目录指针），原子写 |
| `src-tauri/src/deeplink.rs` | `deeptutor://` / `file://` / 裸路径 → 路由或文件的**纯分类器**（含拒绝规则） |
| `src-tauri/src/handoff.rs` | hand-off 队列（FIFO、上限 8、成对记录来源） |
| `src-tauri/src/notify.rs` | 通知"投递目标"记账（谁被通知了、还没被领走） |
| `src-tauri/src/window.rs` | 主窗口改为**代码创建**：`on_navigation`（外链交给系统浏览器）、`on_download`（落 Downloads）、关闭 Tauri 自带拖放处理器 |
| `src-tauri/src/app.rs` | 菜单栏加"检查更新…"与两个勾选项；托盘加"检查更新…"；`sync_preference_checks` 保持勾选状态真实 |
| `src-tauri/src/supervisor.rs` | 首启闸门、偏好读写、通知投递、hand-off 入队、更新检查（安装版 / 只读版） |
| `plugins/tauri-plugin-deeptutor/src/lib.rs` | 新增 13 个插件命令（见 §1.3） |
| `src-tauri/tauri.conf.json` | `deep-link` 方案 + `fileAssociations`（pdf/epub/md）+ 窗口声明搬进代码 |
| `web/index.html` | splash 增加首启向导（4 步，中英双语，无需 Node 构建） |

### 1.2 前端（web/）

| 文件 | 作用 |
| --- | --- |
| `lib/desktop-shell.ts` | `isDesktopShell()` + 全部外壳命令的类型化封装；浏览器里安全 no-op |
| `features/desktop/DesktopBridge.tsx` | 排空 hand-off 与通知目标；把文件转成 `File` 交给编辑器 |
| `features/desktop/round-notification.ts` | 回合完成通知的文案与触发条件（后台 + 有 session） |
| `features/settings/sections/DesktopSettingsSection.tsx` | `/settings/desktop`：关窗/通知开关、运行时信息、重启服务、日志、检查更新 |
| `features/chat/ChatStateAdapter.tsx` | 在 `done(completed)` 处触发通知（唯一改动点） |
| `features/chat/components/ChatWorkspace.tsx` | 监听桌面 hand-off，喂给既有的 `handleAddFiles` |
| `features/settings/sections/AboutSettingsSection.tsx` | `desktop` 安装模式显示正常（此前 label 缺失），并链到"更新设置" |
| `locales/{en,zh}/app.json` | 27 条新文案（两语言键集合一致） |

### 1.3 新增插件命令（远程源可调用面）

`shell_settings`、`update_shell_settings`、`notify_round_complete`、
`take_notification_target`、`take_open_request`、`first_run_state`、`apply_first_run`、
`check_updates`、`log_event`、`pick_files`、`pick_folder`、`reveal_in_folder`、
`read_local_file`、`restart_app`。

全部登记在 `plugins/tauri-plugin-deeptutor/{build.rs,permissions/default.toml}`，
并已由内置探针从 **loopback 真实 UI 源**验证可达（§3.3）——这是 Phase 0 那条
"应用命令不可远程调用"约束的延续。

## 2. 设计决定（含被否掉的方案）

### 2.1 通知"点击回到会话"怎么做到

Tauri 的桌面通知 API 只有 `show()`：macOS 上没有点击回调，通知由系统投递，应用即使隐藏也会收到。
所以外壳保存**意图**（哪条通知指向哪个会话），在应用重新活跃时投递：

1. 窗口重新获得焦点 → UI 通过 `take_notification_target`（destructive take）领走；
2. 应用被激活但窗口在托盘里（`RunEvent::Reopen`，macOS 点 Dock/通知栏）→ 外壳 `reveal()` +
   用 `deeptutor://notification-target` 事件推送同一目标。

"领走即清空"是刻意的：一次通知只能把用户带回一次，焦点抖动或页面重载不会反复把会话拽出来。

**已知边界**：窗口隐藏且用户既不点 Dock 也不点托盘时，"点击通知"只把应用激活，窗口不会被我们自动
显示（macOS 不提供点击回调来区分"点了通知"与"切了个应用"）。托盘图标是这种情况下的回程入口；
该限制已写进 §4.1 的人工点检清单。

### 2.2 深链语法与安全边界

```
deeptutor://chat/1f0c            -> /chat/1f0c
deeptutor://settings             -> /settings
deeptutor://                     -> /
deeptutor://co-writer?doc=42     -> /co-writer?doc=42
file:///Users/x/paper.pdf        -> 文件 hand-off（读字节交给编辑器）
```

- 只接受 `deeptutor` / `file` 两种 scheme；`http(s):` 一律不处理（外链归 `on_navigation`）。
- 每段做白名单字符校验，空段忽略，`..`、`/`、`\`、`%` 直接拒绝。百分号不参与解码：
  应用内所有路由都是纯 ASCII 词，出现 `%` 就说明这条链接不是写给 DeepTutor 的。
  实测 `deeptutor://..%2F..%2Fetc/passwd` 被拒（URL 解析器还会先把点段折叠掉）。
- **不做** `deeptutor://open?path=` 这类"链接直接指定本地文件"的语法：文件只从操作系统
  真正打开文件的那条通道（文件关联、Dock 拖放、argv）进来，避免网页链接成为读本地文件的入口。

### 2.3 首启向导的边界

向导跑在 splash（app origin，不需要 remote 授权），只写**外壳**的状态：

| 步骤 | 落点 | 说明 |
| --- | --- | --- |
| 语言 | `shell.json.locale` | 中/英双语向导本身；应用界面的语言仍归应用自己的"设置 → 通用" |
| 数据目录 | `bootstrap.json`（写在平台默认 home） | 改动需重启才生效，向导给"立即重启 / 稍后再用当前目录"两条路 |
| 桌面行为 | `shell.json` | 关窗到托盘、回合完成通知 |
| 模型/密钥 | 交接给应用的设置页 | 不在向导里重做一遍模型表单 |

为什么数据目录用 `bootstrap.json`：外壳在知道"数据在哪"之前只能读平台默认位置，所以指针必须留在那里；
它指向的目录不存在时会被忽略（不会静默在别的盘上建出一个空 profile），重启后再次询问。

**为什么语言不做全链路写入**：应用界面语言存在自己的 `interface.json`，设置页是"草稿 + 保存"
模式（`useSettings().updateLanguage` 是 staged 的）。外壳绕过它直接改文件或本地存储，会造成
SSR 与客户端语言不一致——正是 `SettingsStore` 注释里记过的"设了中文却用英文回答"那类问题。
所以向导把语言交接给应用设置页，而不是伪造一次保存。

### 2.4 设置页"桌面"分区：降级而不是隐藏

清单写的是"浏览器模式下这些入口隐藏或退化为 Web 行为"。这里选**降级**：`/settings/desktop`
在浏览器里显示一句说明，在桌面外壳里显示真实开关。理由：设置导航是客户端组件，但导航树在 SSR
时也要渲染；用 `isDesktopShell()`（依赖 `window.__TAURI__`）决定某条目是否存在，就是一次必然的
hydration 不匹配。降级同时还有个副作用：Web 用户能看到"桌面版还提供这些"。

## 3. 实测证据

### 3.1 无头流（可在 CI 复跑）

新增四个无头命令：`--first-run-status`、`--complete-first-run`、`--shell-settings`、`--check-updates`
（`--check-updates` 是**只读**的，只报告"有没有新版本"，绝不下载、绝不重启服务）。

```
$ DEEPTUTOR_HOME=/tmp/… deeptutor-desktop --first-run-status
  first_run.completed = false / default_locale = en / can_change_data_dir = true

$ … --complete-first-run --locale en --no-notifications
  restart_required = false；desktop/shell.json 落盘（notifications=false, first_run_completed=true）

$ … --complete-first-run --locale zh-CN --data-dir /tmp/…/custom-data
  restart_required = true
  desktop/bootstrap.json  → /tmp/…/custom-data
  /tmp/…/custom-data/desktop/shell.json 已带上向导答案（重启后不再问）

$ … --first-run-status            # 下一次启动
  home = /tmp/…/custom-data / completed = true / locale = zh-CN

$ … --check-updates --catalog <本地清单元数据>
  runtime.checked = true / updated = false / detail = "有新版本 9.9.9 可用（250 MB）"
  shell.status = unconfigured（外壳自更新未配置签名密钥）
$ … --pack-status                 # 证明第 6 步什么都没装
  active = None / installed = 0
```

### 3.2 首启闸门（真实进程）

```
$ DEEPTUTOR_HOME=<全新目录> deeptutor-desktop        # 打开窗口，什么都不做
[shell] waiting for the first-run wizard to be completed
launcher.log 不存在 / runtime.json 不存在            # 闸门确实拦住了 launcher
```

这条日志是为可诊断性专门加的：没有它，"应用打开了但什么都没发生"在现场没有任何痕迹。

### 3.3 完成向导后的启动 + 远程 IPC 自检（真实进程）

```
[shell] using interpreter …/.venv/bin/python (from <workdir>/.venv)
[shell] remote-ipc probe dispatched (attempt 1)
[shell] webview-ipc ok: invoke received from http://localhost:3782/chat
[shell] remote-ipc probe result: core-app-ok=1.6.10 ;
        shell-cmd-ok=true launch=1 restarts=0 ;
        shell-settings-ok=true/true/zh-CN ;        # ← Phase 3 命令从 loopback 源可达
        restart=ok
[shell] remote-ipc probe result: … shell-settings-ok=true/true/zh-CN
```

`shell-settings-ok=true/true/zh-CN` 同时证明三件事：新命令的 ACL 授权生效、外壳读到了向导保存的
偏好（`close_to_tray=true / notifications=true / locale=zh-CN`）、前端桥（`DesktopBridge`）在真实
UI 里跑起来了。窗口由代码创建（`tauri.conf.json` 里已无窗口声明），菜单/托盘/单实例/窗口状态
插件均正常装配。

退出后 `runtime.json.status = stopped`，没有残留的 `deeptutor_cli.main` 进程。

### 3.4 自动化测试

```
cd desktop && cargo fmt --all -- --check && cargo clippy --locked --all-targets -- -D warnings
             cargo test --locked           → 49 passed
cd web       && npm run typecheck          → ok
             npm run test:unit             → 84 files / 344 passed（含 11 条新用例）
             npm run lint / i18n:check / architecture:check → ok
```

新增 Rust 用例覆盖：设置文件默认值/往返/损坏回退/原子写、bootstrap 指针语义、
ShellConfig 与 bootstrap 的优先级、偏好补丁与通知闸门、向导（闸门、换目录、拒绝相对路径）、
深链分类（路由映射、点段、百分号拒绝、argv 形态）、hand-off 队列（顺序、上限、来源）、
通知目标"只投递一次/最新优先"、导航策略（loopback 内留、外链外开、深链入队）、下载重名改名。

前端用例覆盖：`isDesktopShell()` 的三种情形、通知在（非后台 / 无 session / 浏览器）时静默、
通知请求的路由与会话 id、`read_local_file` → `File` 的字节还原。

### 3.5 顺手修掉的一个真 bug：窗口尺寸/位置其实从未被恢复

Phase 1 起就把"记窗口状态"算作已完成，但 `tauri-plugin-window-state` 只在它的
`on_window_ready` 钩子里恢复状态，而 **Tauri 2.11.6 从不调用这个钩子**（全仓库检索确认）。
插件依然会在窗口事件与退出时*保存*状态，所以磁盘上有文件、表现上"像是记住了"，实际每次启动
都用配置里的默认尺寸。

修复：`window.rs` 建完窗口后显式调 `restore_state(StateFlags::all())`。

验证（预置一份 1024×640 的状态文件后启动）：

```
[shell] remote-ipc probe result: … shell-cmd-ok=true launch=1 restarts=0 win=1024x640 …
```

为了让这条结论**可观测**，`DesktopStatus` 增加了 `window` 字段（宽高、位置、最大化/全屏/可见），
内置探针把它打进 `win=WxH`。没有这个字段时，"窗口忘记尺寸"在日志里完全看不出来。

## 4. 未完成项与人工点检

### 4.1 必须人工点检（无法无头验证）

1. **托盘图标外观**：菜单/行为已代码化，但"系统托盘里长得对不对"只能看图。
2. **系统通知横幅**：首次触发时 macOS 会弹权限询问；确认横幅文案、点击后的回程行为（§2.1 的边界）。
3. **WebView 高风险面**（Phase 0/2 遗留）：PDF 阅读器、EPUB、Finder 拖拽上传、导出下载。
   本轮为导出下载补了确定性行为（固定落 `~/Downloads` + 重名改名 + 完成事件 + 可在文件夹中显示），
   但"PDF/EPUB 在 WKWebView 里渲染正常"仍需肉眼确认。
4. **`deeptutor://` 与文件关联**：需要**已打包**的应用（`tauri build`）才会写进 Info.plist /
   Windows 注册表；开发版二进制不注册。打包后请验证：浏览器地址栏打开 `deeptutor://settings`、
   双击 PDF 选"用 DeepTutor 打开"、把 PDF 拖到 Dock 图标上。

### 4.2 明确留到后面

- **外壳自更新**（`tauri-plugin-updater`）：与 Phase 2 相同，缺签名密钥与真实 release 资产。
  菜单里的"检查更新…"目前诚实地回答"外壳自更新尚未配置"，不会假装"已是最新"。
- **可选扩展按需下载**：运行时胖包已含核心依赖；`scripts/install_extras.py` 那类按需下载需要
  网络与磁盘预算 UI，留 backlog。
- **数据目录迁移（已有 profile 换位置）**：向导只在首次启动时允许设数据目录；已有 profile 的搬迁
  走 Phase 2 的 workspace 迁移路径，不在本阶段。

## 5. 影响面（Web / CLI 不变）

- Python 侧本阶段**零改动**：向导、通知、hand-off 全部由外壳 + 前端完成，`deeptutor_cli` /
  launcher 的参数与默认行为逐字节不变（`tests/runtime` 回归未动）。
- 前端所有新增入口都以 `isDesktopShell()` 为闸门，浏览器里 `DesktopBridge` 直接 return、
  通知函数直接 return、`lib/desktop-shell.ts` 的所有调用抛出可读错误而不是碰 `window.__TAURI__`。
- 新增文案两语言键集合完全一致（`i18n:parity` 通过），没有出现只在一种语言里存在的键。
