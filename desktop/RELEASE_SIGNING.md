# 桌面发布：密钥、签名与更新通道

`.github/workflows/desktop-release.yml` 的每一步签名都由仓库 secret 驱动。本文说明每个 secret
是什么、怎么生成、以及**缺失时的降级行为**——最后一栏才是重点：发布流水线不会因为缺密钥而假装
成功。

| Secret | 用途 | 缺失时的行为 |
| --- | --- | --- |
| `TAURI_SIGNING_PRIVATE_KEY` | 签名外壳更新产物（`latest.json` 的唯一凭据） | 构建加 `--no-sign`；产物可安装但**没有更新通道**：不发布 `latest.json`，已装用户永远看不到这次发布 |
| `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | 上面这把私钥的口令 | 私钥无口令时留空即可（CI 用空口令） |
| `APPLE_CERTIFICATE` | Developer ID Application 证书（`.p12` 的 base64） | macOS 产物只做 **ad-hoc 签名**：本机能跑，别的机器被 Gatekeeper 拦 |
| `APPLE_CERTIFICATE_PASSWORD` | 上面证书的导出密码 | 同上 |
| `APPLE_SIGNING_IDENTITY` | 可选；必须与证书里的 identity 一致 | 省略时用导入证书的 identity |
| `APPLE_ID` / `APPLE_PASSWORD` / `APPLE_TEAM_ID` | 公证（app-specific password 方式） | 跳过公证与 stapling |
| `APPLE_API_KEY` / `APPLE_API_ISSUER` / `APPLE_API_KEY_PATH` | 公证（App Store Connect API Key 方式，可替代上一行） | 同上 |
| `WINDOWS_CERTIFICATE` | 代码签名证书（`.pfx` 的 base64） | 安装包未签名（SmartScreen 警告） |
| `WINDOWS_CERTIFICATE_PASSWORD` | 上面证书的密码 | 同上 |

## 1. 更新通道密钥（当前已生成，请务必备份）

已在本机生成，**公钥已经写进** `desktop/src-tauri/tauri.conf.json` 的 `plugins.updater.pubkey`：

| 文件 | 内容 | 处置 |
| --- | --- | --- |
| `~/.tauri/deeptutor-updater.key` | 私钥（无口令） | **备份到密码管理器**；只作为 GitHub secret 使用，不要进仓库 |
| `~/.tauri/deeptutor-updater.key.pub` | 公钥 | 已提交（就是配置里那一行） |

```bash
# 一次性：把私钥写进仓库 secret
gh secret set TAURI_SIGNING_PRIVATE_KEY < ~/.tauri/deeptutor-updater.key

# 公钥（应该与 tauri.conf.json 里的字符串完全一致）
cat ~/.tauri/deeptutor-updater.key.pub
```

> ⚠️ **私钥丢了等于更新通道死掉**：已经发布出去的应用只认这个公钥，无法用新私钥签名。
> 唯一的补救是发一个"用户手动安装"的版本，里面换上新公钥——所以请立刻备份。
> 反过来，私钥泄露则任何人都能伪造更新，必须走同样的换钥流程。

轮换密钥时：`tauri signer generate -w ~/.tauri/deeptutor-updater-new.key` → 更新配置里的 `pubkey`
→ 发布一个需要用户手动安装的版本 → 之后的自动更新用新钥。

## 2. Apple 证书与公证

```bash
# 从钥匙串导出 Developer ID Application 证书为 .p12（含私钥），然后：
base64 -i DeveloperIDApplication.p12 | pbcopy     # → APPLE_CERTIFICATE
gh secret set APPLE_CERTIFICATE
gh secret set APPLE_CERTIFICATE_PASSWORD
gh secret set APPLE_SIGNING_IDENTITY   # 例如 "Developer ID Application: Name (TEAMID)"，可省略

# 公证凭据（app-specific password，不是 Apple ID 登录密码）
gh secret set APPLE_ID
gh secret set APPLE_PASSWORD
gh secret set APPLE_TEAM_ID
```

流水线里的落地位置：

1. **签名 + 公证 + staple `.app`**：`tauri build` 的 macOS app 步骤内部完成（`APPLE_CERTIFICATE*`
   管签名，`APPLE_ID/PASSWORD/TEAM_ID` 管公证，默认 staple）；
2. **dmg 额外一轮**：Tauri 不公证 dmg 本身，工作流补 `notarytool submit --wait` + `stapler staple`；
3. **验证**：`codesign --verify --deep --strict`、`xcrun stapler validate`、`spctl --assess`，任一不过即失败。

## 3. Windows 证书

Tauri 的 Windows 签名读的是**配置里的证书指纹**，不是文件，所以工作流先导入再传
`--config desktop/src-tauri/windows-signing.json`：

```bash
base64 -i certificate.pfx | pbcopy      # → WINDOWS_CERTIFICATE
gh secret set WINDOWS_CERTIFICATE
gh secret set WINDOWS_CERTIFICATE_PASSWORD
```

构建后用 `Get-AuthenticodeSignature` 逐个校验安装包，签名无效即失败。

## 4. 无证书时的本地演练（已实测）

```bash
# .app + 真实 Info.plist（深链与文件关联都在里面）
cd desktop/src-tauri
tauri build --bundles app --no-sign
plutil -p target/release/bundle/macos/DeepTutor.app/Contents/Info.plist | grep -A3 CFBundleURLSchemes

# 用本机更新密钥产出**签名**的 updater 产物
TAURI_SIGNING_PRIVATE_KEY="$(cat ~/.tauri/deeptutor-updater.key)" \
  tauri build --bundles app --config /tmp/deeptutor-updater-test.json
# → bundle/macos/DeepTutor.app.tar.gz + .sig
```

## 5. 更新通道自检（无头，CI 或发布后均可跑）

```bash
# 只报告，不下载：运行时包 + 外壳两条通道
deeptutor-desktop --check-updates

# 下载并校验签名，但不安装（发布后验证资产是否可用）
deeptutor-desktop --verify-shell-update

# 真的装上（脚本 / 支持流程用；GUI 会先问用户）
deeptutor-desktop --install-shell-update
```

判据：`--check-updates` 的 `shell_update.status` 为 `available` / `up_to_date`；`--verify-shell-update`
失败即说明签名与 `pubkey` 不匹配（产物坏了，或换了公钥却没重新签名）。

两个实测发现的坑，写下来省下一次排查：

- **macOS 更新器拒绝符号链接路径**：`/tmp` 在 macOS 上是 `/private/tmp` 的符号链接，从
  `/tmp/.../DeepTutor.app` 运行时会报 `current_exe() that contains a symlink`。本地演练请用
  `/private/tmp/...` 或用户目录下的真实路径。
- **`.sig` 是 base64 包着 minisign**：`latest.json` 里的 `signature` 就是 `.sig` 文件的内容，
  而该文件本身是 base64 编码的 minisign 文本。`desktop/scripts/assemble_updater_manifest.py`
  会解码回来校验结构，粘一份未编码的 minisign 文本会被拒。
