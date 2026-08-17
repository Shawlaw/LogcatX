# 应用更新签名与发版说明

LogcatX 的应用内更新基于 [DeskFoundry](https://github.com/Shawlaw/DeskFoundry) 的 `desktop-updater` 契约：

- 客户端只信任 **编译期注入的 Ed25519 公钥**（环境变量 `LOGCATX_UPDATE_PUBLIC_KEY`）。未注入的构建会显示“此构建尚未配置应用更新”，且完全不发起更新请求。
- 更新清单托管在仓库 `master` 分支的 `updates/stable.json`，附带分离式签名 `updates/stable.json.sig`，由发版流水线在打 tag 后自动签名提交。
- 更新包为发布页的绿色版 zip，下载后先做 SHA-256 / 大小校验，再由 `LogcatX.Updater.exe` 在应用退出后替换白名单文件（`desktop-update.toml`）并重启，新版本启动成功前保留回滚副本。

## 一次性配置签名密钥（维护者操作）

1. 生成密钥对（在 DeskFoundry 仓库检出中执行）：

   ```bash
   git clone https://github.com/Shawlaw/DeskFoundry.git
   cd DeskFoundry
   cargo run --locked -p desktop-update-publisher -- keygen
   ```

   输出形如：

   ```text
   private_key_base64=...   # 私钥：仅放入 GitHub Secret，绝不能提交进仓库
   public_key_base64=...    # 公钥：编译期注入客户端，可公开
   ```

2. 配置到 GitHub 仓库（`Shawlaw/LogcatX`）：

   ```bash
   gh secret set DESKTOP_UPDATE_PRIVATE_KEY --body "<private_key_base64>"
   gh variable set LOGCATX_UPDATE_PUBLIC_KEY --body "<public_key_base64>"
   ```

   - `DESKTOP_UPDATE_PRIVATE_KEY`（Actions secret）：发版时用于签名清单。
   - `LOGCATX_UPDATE_PUBLIC_KEY`（Actions variable）：构建时注入客户端，并作为签名结果的校验基准。

3. 配置完成后无需改代码。下一次打 `v*` tag 发版时：

   - 构建出的 `LogcatX.exe` 即具备完整的检查/下载/应用更新能力；
   - `release.yml` 末尾的 `publish-portable-update` 步骤会生成并提交 `updates/stable.json(.sig)` 到 `master`。

> 密钥未配置时流水线仍然可用：发版正常进行，manifest 发布步骤自动跳过，产物只是不带更新能力。**注意：某个版本若在未配置公钥时构建发布，该版本的用户无法应用内升级，需要手动下载下一次的 zip。** 因此请先配置密钥、再打第一个携带更新能力的 tag。

## 发版流程（每次版本）

1. 更新 `Cargo.toml` 的 `version`，并在 `CHANGELOG.md` / `CHANGELOG.en.md` 顶部补充 `## [版本号] - 日期` 小节（GitHub Release 文案将从中提取）。
2. 提交后打 tag 并推送：

   ```bash
   git tag -a v<版本号> -m "Release v<版本号>"
   git push origin master v<版本号>
   ```

3. `release.yml` 在 `windows-latest` 上：校验 tag 与 `Cargo.toml` 一致 → 跑测试 → `scripts/package_windows_release.sh` 产出
   `dist/LogcatX_<版本>_windows_x64_portable_<提交号>.zip` → 从 CHANGELOG 提取 notes → 创建 GitHub Release → （配置了密钥时）签名并提交更新清单。

## 布局白名单的同步要求

以下三处必须保持一致，改动任何一处都要同步其余两处：

1. 仓库根目录 `desktop-update.toml` 的 `replace_files`（发布工具校验 zip 内容）；
2. `src/updater.rs` 的 `RELEASE_REPLACE_FILES`（客户端替换白名单）；
3. `scripts/package_windows_release.sh` 实际打进 zip 的文件。

zip 中多出或缺少任一声明文件，发版的清单签名步骤都会直接失败——这是有意的防回退/防夹带设计。

## 本地体验更新流程（demo 预览）

调试构建（或任何带 `--features update-preview` 的构建）支持演示模式，用来看看检查更新、下载并重启更新的完整体验，而不需要发布任何真实版本：

```bash
LOGCATX_DEMO_APP_UPDATE=1 cargo run
```

行为说明：

- 窗口首次获得焦点约 0.7 秒后，模拟出一个 `9.9.9-demo.1` 候选版本：版本徽章亮起提示点并弹出更新弹窗（弹窗内有明显的“演示预览”横幅）；也可以在弹窗里手动点“检查更新”。
- 全程不联网、不写更新状态缓存，与真实检查互不影响；release 构建（未开 feature）下该环境变量被完全忽略。
- “下载更新”会在本地合成一个符合 `desktop-update.toml` 白名单的更新包（`README.md` 会在包内附加一行演示标记）；“重启并更新”走的是真实的 helper 替换、重启、回滚保护与启动确认流程。
- **体验完整应用链路需要发布版布局的目录**（包含 `LogcatX.exe`、`LogcatX.Updater.exe` 及全部白名单文件）。推荐做法：把 `dist/` 下打包脚本产出的 zip 解压到临时目录，用 `target/debug/logcatx.exe`、`target/debug/logcatx-updater.exe` 覆盖其中的 `LogcatX.exe`、`LogcatX.Updater.exe`，再从该目录设置环境变量启动。直接在 `target/debug` 里运行时，检查与提示可用，点“下载”会因布局缺失给出明确报错。
- 应用重启后环境变量会被 helper 继承，因此会再次进入演示循环（可随时关掉）；更新成功后打开该目录的 `README.md` 能看到演示标记，说明替换真实发生。
