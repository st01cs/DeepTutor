# Phase 4 进展报告

日期：2026-09-25 · 范围：运行时包增量更新（Spike B）、Linux 接入准备、启动性能埋点

## 0. 结论

Phase 4 的头号项——**增量运行时更新**——已经跑通并端到端验证：

| 项 | 计划目标 | 实测 |
| --- | --- | --- |
| 常见更新下载量 | 从 300MB 降到 10–30MB | **2.2 MB**（一次真实版本对的增量：换 wheel + 前端整体重建）；只改 manifest 的那次是 **0.2 MB** |
| 更新完整性 | 结果与完整包一致 | 独立实现逐文件比对：**0 行差异**，总字节数完全一致（753,623,848） |
| 失败保护 | 基线不匹配要拒绝 | 拒绝并**自动回退完整包**（实测：旧版安装器留下的基线被拒，更新照样成功） |
| 回滚 | 保持可用 | `--rollback-pack` 回到旧包，解释器随之切换 |

另外两项（启动埋点、Linux 打包准备）也落地了；Windows/Linux 体验打磨按计划留到有对应平台时再做。

## 1. 增量更新怎么工作

```
build_pack.py（1.6.10）      build_pack.py（1.6.11）
        └── stage-1.6.10 ──┬── stage-1.6.11
                           │
                  build_delta.py
                           │  只打包"变了的东西"
                           ↓
        1.6.11-macos-aarch64.delta.tar.gz  (2.2 MB)
        ├── delta.json     # 基线指纹、目标指纹、增/改/删清单、暂存根路径
        ├── files/**       # 仅新增/变更的文件（原始字节）
        └── links.json     # 变更的符号链接
                           ↓
        assemble_catalog.py 把 delta 挂到对应 pack 条目上
                           ↓
        shell: update_from_catalog 发现基线匹配 → 优先走增量
```

外壳侧（`runtime_pack.rs::install_delta_archive`）的顺序刻意保守：

1. 校验整包 sha256（不匹配直接拒，不落盘）；
2. 解包并**先检查全部路径**（绝对路径 / `..` 一律拒绝），一个坏路径不会留下半成品；
3. 当前活动包必须正好是这个增量的基线（pack_id + 文件数 + 归一化指纹）；
4. 用**硬链接**克隆基线（不占额外空间），再把增量文件逐个原地替换；
5. 每个增量文件在放入前校验 sha256（用**传输的原始字节**，不是指纹摘要）；
6. 应用后再算一次目标指纹，必须等于构建机记录的目标指纹；
7. `rehydrate` → 冒烟 → 原子改名就位 → 记录 `previous_pack`。

任何一步失败都只影响暂存目录，活动包原封不动。

## 2. 指纹：整件事里唯一微妙的地方

增量只在"基线正好是构建机用的那棵树"时才成立，所以两端必须能把树指纹对上。而**包一装上就变了样**：

- `rehydrate` 会把 `pyvenv.cfg` 的 `home`、三个解释器链接、以及 venv 里所有"提到构建机路径"的小文本文件改写成安装后的路径；
- 但 `rewrite_prefix` **只遍历 venv**：CPython 树、`manifest.json`、wheel 自己的元数据里那个构建路径会**永远留着**。

于是 `pack_tree::tree_digest` 的规则是：

| 规则 | 原因 |
| --- | --- |
| 大于 256 KiB 的文件按原样哈希 | `rewrite_prefix` 也不碰它们 |
| 小文本文件里的"包根路径"替换成 `<PACK_ROOT>` 再哈希 | 这正是 rehydrate 做的事 |
| 额外 token 化**构建机记录的根路径**（`delta.base_root`/`target_root`） | venv 之外的文件永远带着它 |
| 跳过 `venv/bin/python{,3,3.12}` | rehydrate 改的是它们的*形态*（绝对 → 相对），不是路径 |
| 跳过 `__pycache__` / `*.pyc` / `.DS_Store` | 装完跑一次就会出现，不能算内容 |
| size 与 hash 取自**同一份字节** | 归一化会改变长度，混用会凭空多出 108 字节（真的踩过） |

Python（构建侧）与 Rust（安装侧）各有一份实现，两边用同一组固定件的断言 + 真实 764 MB 包互相对账。

## 3. 实测记录

### 3.1 真实增量（1.6.10 → 1.6.11，含 wheel 与前端整体重建）

```
$ python3 desktop/pack/build_delta.py --base …/stage-1.6.10-macos-aarch64 --target …/stage-1.6.11-macos-aarch64
[delta] delta 1.6.10-macos-aarch64 -> 1.6.11-macos-aarch64: +601 changed, -10 removed, 0 links
[delta] archive 1.6.11-macos-aarch64.delta.tar.gz: 2.2 MB
完整包对照：257.5 MB（归档）/ 764 MB（展开）；28,985 个文件中只有 611 个动过
```

### 3.2 目录驱动的更新（用户真实路径）

```
$ deeptutor-desktop --update-pack --catalog <本地 catalog>
[runtime-pack] updating 1.6.10-macos-aarch64 -> 1.6.11-macos-aarch64 with a delta (2 MB instead of a full pack)
[runtime-pack] delta 1.6.10-macos-aarch64 -> 1.6.11-macos-aarch64: 39 added, 562 changed, 10 removed (unpacked in 0.2s)
[runtime-pack] delta: base fingerprint 28985 files / 717 MB in 24.4s
[runtime-pack] delta: base cloned (hard links) in 32.0s
[runtime-pack] delta: applied and re-fingerprinted in 57.2s
[runtime-pack] runtime pack 1.6.11-macos-aarch64 installed from delta off 1.6.10-macos-aarch64
  updated: True | active: 1.6.11-macos-aarch64 | previous: 1.6.10-macos-aarch64      # 共 59.7s

$ deeptutor-desktop --update-pack --catalog …      # 再跑一次
[runtime-pack] active pack 1.6.11 is already the newest for macos-aarch64
  updated: False
$ deeptutor-desktop --rollback-pack
[runtime-pack] rolled back to runtime pack 1.6.10-macos-aarch64
```

独立校验（另一套 Python 实现 + 正确 token 集）：增量装出来的树与完整包解出来的树 **0 行差异**、总字节数相同；`--self-check --require-python` 解析到新包的解释器，`import deeptutor_cli.main, deeptutor_web` 通过，`node v20.18.0` 在位。

### 3.3 时间与带宽的取舍（诚实版）

| 路径 | 下载 | 本地耗时 |
| --- | --- | --- |
| 完整包 | 257 MB | 24s（安装） |
| 增量 | 2.2 MB | 60s（24s 基线指纹 + 32s 硬链接克隆 + ~4s 覆盖/再指纹/冒烟） |

按 40–50 Mbit/s 折算，增量在**更慢的链路上更快**；在千兆级链路上完整包反而更省时间。因此目录里两种路径都保留，由基线是否匹配决定（匹配则增量，不匹配自动回退完整包）。
明显的下一步优化（未做）：省掉"应用后再指纹一次"（约 −24s）与并行化克隆（约 −20s），代价是牺牲"结果 == 目标"这一条最强断言。

## 4. 顺手修掉的真 bug（都是增量机制逼出来的）

1. **安装包指向临时目录**：`install_archive` 先 `rehydrate` 再 `rename`，于是每个装好的包 `pyvenv.cfg` 的 `home`、`direct_url.json` 都指向已经不存在的 `.incoming-<pid>/`。现在改成"就位之后再 rehydrate + 冒烟"。旧安装会在下一次完整安装时自愈；对着它们做增量会被指纹拒绝并回退完整包（实测）。
2. **`--app-version` 可以给包贴错版本**：wheel 版本来自 `deeptutor/__version__.py`，`--app-version` 只改归档名，于是出现过"1.6.11 的包跑的是 1.6.10"——所有校验都自洽，只有用户会发现"更新完没变化"。现在构建时校验 wheel 版本并直接报错。
3. **`assemble_catalog.py` 会把增量片段当成 pack**：`*.catalog.json` 也匹配 `<pack>.delta.catalog.json`，一有增量发布就会炸。已排除并加测试。
4. **文件式目录不可用**：`install_from_url` 只走 `ureq`，而目录里的 URL 会解析成本机路径——离线镜像/本地目录"能读不能用"。现在两条通道都支持本地路径。
5. **增量条目用了指纹摘要而不是原始字节**：安装器因此拒绝自己刚下下来的文件。已区分"指纹摘要"与"载荷摘要"，并加测试钉住。

## 5. 启动性能埋点

外壳新增 `startup` 计时（`desktop_status.startup`，并写一行日志），页面加载后前端用 `note_ui_ready` 上报首屏时刻：

```
[shell] startup: spawn 1528 ms, ready 4827 ms, first paint 4897 ms (70 ms after ready)
```

结论很明确：**首屏不是瓶颈**（ready → 首屏 70 ms），时间花在

- 解释器探测与拉起：1.5s（每个候选都要 `import deeptutor_cli.main` 实测，冷启动更慢）；
- 后端 + Next standalone 起来：约 3.3s。

下一步优化方向因此是"缓存解释器探测结果 + 后端延迟导入"，而不是动 WebView。

## 6. Linux 接入（做到哪一步）

| 项 | 状态 |
| --- | --- |
| 运行时包构建支持 `linux-x86_64` / `linux-aarch64` | ✅ 键位、PBS triple、Node 包、`host_platform()` 都补齐 |
| CI 里跑 Linux 包 | ✅ `packs-linux`（`workflow_dispatch` 的 `include_linux`，ubuntu-22.04） |
| catalog 平台键 | ✅ `linux-x86_64` 与外壳 `host_platform()` 一致 |
| Linux 外壳（deb/rpm/AppImage） | ⏸ 未做：需要 WebKitGTK 宿主依赖说明、托盘图标、`.desktop` 文件与 `tauri build --bundles deb,rpm` 的 CI 作业 |
| 本地验证 | ⏸ 本机没有 Linux runner，这一步只能由 CI 证明 |

## 7. 未做 / 已知限制

- Windows/Linux 体验打磨（任务栏、字体、WebView2 兜底）留到有对应平台时做。
- 增量包的**生成**在 CI 里还没有作业：它需要"上一次发布"的包作为基线（`gh release download <prev>`），我这次用的是本地两份 staged 树。目录/安装/回退三段都已就绪，缺的是把"取上一个 release 的包 → 生成 delta → 上传"接进 `desktop-release.yml`。
- 增量更新目前不做**多基线**：一个版本只对一个基线有效，跨两个版本安装的机器走完整包。
- `desktop/pack/dist/` 里保留着我这次测试用的 2.1 GB staged 树与归档（已在 .gitignore 内），可随时删除。
