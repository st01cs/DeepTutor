# Phase 2 进展报告

日期：2026-09-24 · 范围：运行时包、安装/更新/回滚、发布流水线

> **收尾（同日第二轮）**：外壳自更新通道已接通并端到端实测，发布流水线补齐真正的
> dmg/nsis/msi、签名、公证、验证与 `latest.json`。详见文末 §8。

## 0. 结论

Phase 2 的核心链路已经**真实跑通**：一个不含任何用户依赖的运行时包（自带 CPython、venv、
Node、前端产物）可以在纯净环境下装起来并启动 DeepTutor；更新与回滚都在本机端到端验证过；
发布用的清单与打包脚本已就位。

| 项 | 状态 |
| --- | --- |
| `desktop/pack/build_pack.py` + `runtime.lock.txt` | ✅ 实测产出 762MB 树 / **257MB** 归档（macOS arm64） |
| 胖包直发（本地归档安装） | ✅ `--install-pack` 实测 24s |
| 联网安装（`local`/`remote` 走同一段逻辑） | ✅ 本机 HTTP 源实测下载 257MB → 校验 → 安装 |
| 摘要校验拒绝坏包 | ✅ 错误 sha256 在写入前被拒（exit 1） |
| 原子切换 / 幂等 / 回滚 | ✅ 更新 → 再检查（already newest）→ 回滚，状态全程正确 |
| 零依赖启动 | ✅ 纯净环境（无任何 `DEEPTUTOR_*`，PATH 仅 `/usr/bin:/bin`）下启动成功 |
| 版本单一真源 | ✅ `desktop/scripts/sync_version.py`（含 `--check` 守卫） |
| shell 单测 | ✅ 18 passed（清单/摘要/路径穿越/rehydrate/切换/回滚） |
| macOS 签名+公证 / Windows 签名 | ⛔ 需要证书（流水线已写好，未执行） |
| `tauri-plugin-updater`（外壳自更新） | ⏸ 未做（依赖签名与真实 release 资产） |
| Windows / macOS-x64 包 | ⏸ 代码就绪，需对应 runner（CI 矩阵已列） |

## 1. 包长什么样

```
runtimes/<pack_id>/
├── manifest.json      # schema_version / pack_id / platform / python / node / paths
├── python/            # python-build-standalone 3.12.14 (install_only)
├── venv/              # 可重定位 venv：DeepTutor wheel + 锁定依赖（runtime.lock.txt）
└── node/              # Node 20.18.0 运行时（launcher 通过 PATH 找到它）
```

前端不单独放：`deeptutor_web`（Next standalone 产物）随 wheel 进入 venv，launcher 走它既有的
`_packaged_web_dir()` 路径，把资源复制到 `data/user/runtime/web` 再启动 `server.js`。

实测体积：**762 MB 展开 / 257 MB 归档**（macOS arm64）。构建 13–18s（缓存命中；首次含前端
生产构建约 90s），安装 24s（校验 + 解包 + rehydrate + 冒烟 + 切换）。

## 2. 一个必须记下来的坑：`--relocatable` 不够

`uv venv --relocatable` 只在 `pyvenv.cfg` 写了 `relocatable = true`，**解释器链接和 `home`
行仍是绝对路径**：

```
venv/bin/python -> /build/machine/stage-.../python/bin/python3   # 绝对
pyvenv.cfg: home = /build/machine/stage-.../python/bin           # 绝对
```

把包拷到别处、再删掉旧路径后，`venv/bin/python` 直接不存在（实测）。所以安装时增加
**rehydrate**：

1. 重写 `pyvenv.cfg` 的 `home` 为包内 `python/bin`；
2. 把 `venv/bin/python{,3,3.12}` 重建为相对链接 `../../python/bin/python3`；
3. 把构建机前缀从 venv 内的小文本文件（entry-point shebang、`activate`、`.pth`）里替换掉。

有了它，构建机与用户机的路径**不需要有任何关系**——这是"胖包能在任意机器安装"的前提。

## 3. 端到端验证记录

全部在本机（macOS 27.0 / arm64）执行：

```bash
# 构建（python-build-standalone 3.12.14 + Node 20.18.0 + 锁定 wheel + 前端产物）
python3 desktop/pack/build_pack.py --skip-web-build
# → staged tree 762 MB in 13s；archive 257 MB；sha256 48d7e313…

# 坏摘要：写入前拒绝
deeptutor-desktop --install-pack …tar.gz --sha256 deadbeef
# → 运行时包校验失败：期望 deadbeef, 实际 48d7e313…（已拒绝安装）；exit 1

# 正常安装（本地归档 = 胖包直发）
deeptutor-desktop --install-pack …tar.gz --sha256 48d7e313…
# → runtime pack 1.6.10-macos-aarch64 installed（24s）

# 纯净环境启动（模拟双击：无 DEEPTUTOR_* 变量，PATH 仅 /usr/bin:/bin）
env -i HOME=… PATH=/usr/bin:/bin deeptutor-desktop
# → shell.log:
#   using interpreter …/runtimes/1.6.10-macos-aarch64/venv/bin/python (from runtime pack)
#   runtime pack 1.6.10-macos-aarch64 active; node from …/node/bin
#   remote-ipc probe result: … shell-cmd-ok=true launch=1 restarts=0 ; restart=ok
# → runtime.json: status=ready, frontend_kind=packaged；frontend=200 health=200

# 更新 / 幂等 / 回滚（本地 HTTP 源模拟 GitHub Release）
deeptutor-desktop --update-pack --catalog http://127.0.0.1:8788/runtime-packs.json
# → updated=true active=1.6.11-macos-aarch64 previous=1.6.10-macos-aarch64（24.8s）
deeptutor-desktop --update-pack --catalog …    # → updated=false（already the newest）
deeptutor-desktop --rollback-pack              # → active=1.6.10-macos-aarch64
deeptutor-desktop --pack-status
# → active:1.6.10-macos-aarch64 previous:1.6.11-macos-aarch64 installed:[1.6.10, 1.6.11]
```

加上外壳静态检查：`cargo fmt --check` 通过、`cargo clippy -D warnings` 无警告、`cargo test`
**18 passed**。

## 4. 新增文件

| 路径 | 作用 |
| --- | --- |
| `desktop/pack/runtime.lock.txt` | `uv pip compile` 锁定的依赖（cli+server extras，Python 3.12） |
| `desktop/pack/build_pack.py` | 按平台构建运行时包，产出 `.tar.gz` / `.sha256` / `.catalog.json` |
| `desktop/pack/assemble_catalog.py` | 合并各平台 fragment → `runtime-packs.json` |
| `desktop/src-tauri/src/runtime_pack.rs` | 清单解析、摘要校验、安全解包、rehydrate、冒烟、原子切换、回滚、更新选择 |
| `desktop/scripts/sync_version.py` | `__version__.py` → Cargo/tauri.conf 版本同步与 `--check` 守卫 |
| `.github/workflows/desktop-release.yml` | validate → packs（三平台矩阵）→ catalog → shell bundle |

外壳新增无头子命令（同时也是这些 API 的调用方，避免"写了没人用"）：

```
--self-check [--require-python]        解析结果 + 解释器候选
--pack-status                          当前/上一个/已安装的包
--pack-catalog <url|path>              查看清单里适用于本机的包
--install-pack <file|url> [--sha256 …] 安装（胖包直发 / 联网）
--update-pack --catalog <url|path>     按清单更新（幂等）
--rollback-pack                        回到上一个包
```

## 5. 遗留

1. **签名与公证**：需要 Apple Developer ID 与 Windows 代码签名证书；`desktop-release.yml`
   里已按 secret 条件写好，但**未执行验证**。
2. **`tauri-plugin-updater`（外壳自更新）**：它与运行时包更新是两条独立通道；等真实 release
   资产与签名密钥就绪后再接，避免现在写一套无法验证的更新 UI。
3. **macOS x64 / Windows 包**：`build_pack.py` 已按平台分支（`.zip` / `Scripts/python.exe` /
   Node win-x64），但只在 macOS arm64 上实测过，另两个由 CI 矩阵覆盖。
4. **四个 WebView 高风险面**（PDF / EPUB / 拖拽 / 导出下载）仍是人工待办。
5. 归档目前是 `.tar.gz`；换 `.tar.zst` 预期再省约 15%，manifest 里已留格式字段。

## 8. 收尾（2026-09-24 第二轮）

§5 的第 1、2、3 条这次全部动过：能验证的都验证了，验证不了的原因也变了（从"没写"变成
"缺证书/缺 runner"）。

### 8.1 外壳自更新通道（§5 第 2 条）已接通

| 项 | 落点 |
| --- | --- |
| 签名密钥 | `~/.tauri/deeptutor-updater.key`（私钥，待写入 CI secret）；公钥已提交进 `tauri.conf.json` |
| 通道配置 | `plugins.updater.endpoints` → `releases/latest/download/latest.json`；`bundle.createUpdaterArtifacts: true` |
| 检查 | 菜单/托盘"检查更新"、设置页按钮、`--check-updates` 一次性报告**两条通道** |
| 安装 | 确认对话框 → 下载 → **验签** → 安装 → 收摊本地服务 → 重启应用；`--install-shell-update` 供脚本使用 |
| 清单 | `desktop/scripts/assemble_updater_manifest.py`（逐平台合并，含结构校验）+ 8 条单测 |
| 文档 | [`RELEASE_SIGNING.md`](RELEASE_SIGNING.md)：每个 secret 的含义与**缺失时的降级行为** |

端到端实测（本机 1.6.10 → 1.6.11，走本机 HTTP 源）：

```
$ deeptutor-desktop --check-updates
  runtime  未配置运行时更新源（local 测试未设 pack catalog）
  shell    status=available  current=1.6.10  available=1.6.11

$ deeptutor-desktop --verify-shell-update        # 下载 + 验签，不安装
  已下载并校验 1.6.11（6 MB，未安装）            exit 0

$ <把产物改一个字节> deeptutor-desktop --verify-shell-update
  外壳更新校验失败: The signature verification failed     exit 1   ← 篡改被拒

$ /private/tmp/…/DeepTutor.app/Contents/MacOS/deeptutor-desktop --install-shell-update
  已安装 1.6.11，重启应用后生效
  Info.plist: 1.6.10 → 1.6.11；替换后的 bundle 仍可运行
```

顺手修掉两个会真出事的坑：`generate_context!()` 展开两次导致链接期
`_EMBED_INFO_PLIST` 重复定义（无头入口引入的）；以及 `parse` 早期版本把 `.sig` 当纯文本，
而 Tauri 写的是 **base64 包着 minisign**，校验方式已按插件实现对齐。

### 8.2 发布流水线（§5 第 1、3 条）

原来的 shell job 跑的是 `tauri build --no-bundle`——也就是说**从来没有产出过 dmg/nsis/msi**。
现在：

| 项 | 现状 |
| --- | --- |
| 产物 | macOS arm64 / macOS x64（dmg + updater tar.gz）/ Windows x64（nsis + msi） |
| 签名 | macOS 由 bundler 用 `APPLE_CERTIFICATE*` 签名并公证+staple `.app`；dmg 额外一轮 notarize+staple；Windows 导入 PFX 后把指纹写进 `--config` |
| 验证 | `codesign --verify --deep --strict` + `stapler validate` + `spctl --assess`；Windows `Get-AuthenticodeSignature` 全部必须 Valid，否则 job 失败 |
| 无证书时 | macOS ad-hoc 签名并 `::warning`；不发布 `latest.json`（不伪造更新通道） |
| 清单 | 新增 `updater` job：合并各平台签名 → `latest.json` → `publish` job 用 `gh release upload` 挂到 release |
| 守卫 | `validate` job 断言 `plugins.updater` 的 pubkey/endpoints/createUpdaterArtifacts 都在，避免发出"永远无法更新"的包 |

本机能验证的那一半已经验证：`DeepTutor.app` 的 Info.plist 里
`CFBundleURLSchemes=["deeptutor"]`、三种文档类型的 `LSItemContentTypes`
（`com.adobe.pdf` / `org.idpf.epub-container` / `net.daringfireball.markdown`）齐全——
顺带发现 tauri-utils 的 UTI 推断表里没有 epub/markdown，已改成显式 `contentTypes`。
**剩余待验证项**：真实证书下的签名/公证、Windows runner 上的 nsis/msi 与签名、x64 运行时包。
