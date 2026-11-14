# AzurPilot 独立版 Windows x64

将简装包装到新目录，打开「AzurPilot 独立版」，在启动器里点「部署」。下载完成后点「启动」。完整包带 Python、ADB、Git、固定版 WebView2 和便携 Node.js，可离线安装并启动。控制台默认在本机的 25549 端口打开。

用过旧版时，先退出旧启动器并备份原目录。在独立版开过一次后，再迁移实例配置和统计。独立版的 `config/deploy.yaml` 指向包内工具，应保留，旧版的部署配置和程序文件请留在旧目录。迁移实例时也需要原来的账号保险库和相关配置，是否可直接迁移取决于原安装使用的版本和存储方式。

## 构建完整安装包

1. 准备自有 AzurPilot 仓库已提交的 `dev` 版本。用该版本的前端锁文件构建 `frontend/dist`，为 Windows x64 准备锁定的 Python 解释器和生产依赖。
2. 准备独立启动器的前端，使用 `npm ci --prefix frontend` 和 `npm run build --prefix frontend`。PowerShell 设置 `$env:TAURI_CONFIG = '{"identifier":"io.github.beatrice-betty.azurpilot.independent"}'`，按下面的命令编译，去掉本机用户目录、源码目录和输出目录。
3. 把 Python 生产依赖放在输入目录的 `python-build/Lib/site-packages`。同一输入目录还需要 `mingit`、`adb/platform-tools`、`atx`、`webview2/<固定版本目录>` 和 Node.js 官方的 `node-v24.22.2-win-x64.zip`。打包时核对 Node.js 的 SHA-256。ATX 使用 openatx v0.10.0 官方文件，VC++ x64 运行库使用微软发行文件。
4. 运行 `tools/build_independent.py --alas <ALAS源码目录> --release <新发布目录> --key <本机发布密钥文件> --package <新装配目录> --inputs <组件输入目录> --python-root <Python解释器目录> --launcher <已编译EXE>`。发布密钥需要保存在仓库及打包目录之外。
5. 生成下文的简装输入目录。为 Inno Setup 设置 `AZURPILOT_PACKAGE_ROOT`、`AZURPILOT_PACKAGE_OUTPUT`、`AZURPILOT_VC_REDIST` 和 `AZURPILOT_FULL_BOOTSTRAP`，分别指向装配目录、输出目录、VC++ x64 安装文件和简装输入的 `bootstrap`。运行 `ISCC.exe install-independent.iss`，输出完整安装程序。

```powershell
$sourceRoot = (Get-Location).Path
$buildRoot = Join-Path (Split-Path -Parent $sourceRoot) 'azurpilot-public-build'
$profilePath = [Environment]::GetFolderPath('UserProfile')
$env:CARGO_TARGET_DIR = $buildRoot
$remaps = foreach ($directory in @($profilePath, $sourceRoot, $buildRoot)) {
    foreach ($prefix in @($directory, $directory.Replace('\', '/'), ('\\?\' + $directory))) {
        "--remap-path-prefix=$prefix=/azurpilot-build"
    }
}
$env:CARGO_ENCODED_RUSTFLAGS = [string]::Join([char]31, @($remaps))
cargo rustc --locked --release --features independent -- -C link-arg=/PDBALTPATH:azurpilot.pdb
```

构建脚本检查启动器中是否还含本机用户目录和 PDB 路径。对外只发安装程序，完整包再附同名数据文件。

## 简装安装包

安装器把启动器、部署工具和默认配置写入选定目录。首次在启动器点「部署」时，从 Git for Windows、Google、Node.js 和 Python 包镜像获取 Git、ADB、Node.js 与锁定的 Python 依赖，从自有 GitCode 或 GitHub 拉取 ALAS。安装目录的 `config/deploy.yaml` 可在部署前修改。

新装简装版用系统 WebView2。系统缺少时，启动器先运行随包的微软 WebView2 引导程序，再打开窗口。缺少对应 VC++ x64 运行库时，「部署」会下载并校验微软安装文件，需要 Windows 管理员授权时由系统弹窗。若显示需要重启，重启后再次打开启动器并点「部署」。

下载失败时在同一窗口重试，「导出启动器日志」可保存失败原因和下载来源。当天日志在 `log/日期_launcher.txt`。部署会复用校验通过的下载。已装环境的启动流程沿用本地组件，想主动重建可先停止服务，再点「修复环境」。修复会重新下载 Git、ADB 和 Node.js，复用校验通过的依赖下载，重建 Python，并补齐本体缺失文件。依赖版本随安装包锁定。

构建简装包需要步骤 4 的整包装配目录和同一发布目录。运行 `tools/build_independent_lite.py --package <整包装配目录> --release <发布目录> --inputs <组件输入目录> --installed <生产依赖site-packages> --local-wheel <锁定源码构建的apkutils2 wheel> --webview2-setup <微软 WebView2 引导程序> --output <简装输入目录>`。组件输入目录保留校验过的 `mingit.zip` 和 `adb.zip` 供装配检查。使用与整包一致的 Windows x64 Python。为 Inno Setup 设置 `AZURPILOT_LITE_ROOT` 和 `AZURPILOT_LITE_OUTPUT`，运行 `ISCC.exe install-independent-lite.iss`。

## 生成带 Windows 代码签名的安装包

用 PowerShell 7 执行 `tools/build_signed_independent.ps1`。简装版传入 `-Edition Lite -PackageRoot <简装输入目录> -OutputRoot <新的输出目录> -ISCC <ISCC.exe路径>`。完整包传入 `-Edition Full -PackageRoot <整包装配目录> -OutputRoot <新的输出目录> -ISCC <ISCC.exe路径> -VCRedist <微软VC++安装文件路径> -BootstrapRoot <简装输入目录的bootstrap>`。需要 Windows SDK 的 SignTool。

该脚本沿用 [Windows 签名工具](windows-signing.md)，对启动器、安装程序和卸载程序签名并验证时间戳。可以传入已有的代码签名凭据。使用本机独立版自签证书时加 `-UseLocalCertificate`，密钥从当前用户的 `%USERPROFILE%/.azurpilot/code-signing` 读取，PFX 密码由 Windows DPAPI 保护。签名后的完整包包含安装 EXE 和同名的 `-1.bin` 数据文件。把两个文件放在同一目录，打开 EXE 即可离线安装。签名完成后重新计算安装文件和数据文件的 SHA-256。自签证书可验证文件完整性，Windows 公共信任需要受信任的签发机构。

## 更新和换源

在本机控制台的 Git 配置里修改「仓库地址」和「分支」，或编辑安装目录的 `config/deploy.yaml`，然后到更新页检查并应用。可填写另一个兼容的 AzurPilot HTTPS Git 仓库。默认使用自有 GitCode 和 `dev`，GitHub 可用于回退。自定义仓库只从用户选的仓库拉取。ALAS 更新直接读取 Git 分支，无需维护者的更新签名或公钥。应用更新会运行该仓库的代码，请选择你信任的仓库。

启动器每次启动 ALAS 前会按所选 Git 分支检查，并在发现新版时只替换 ALAS 程序、资源和网页。检查或应用失败时继续启动已安装版本。可使用 `--skip-update` 临时跳过启动时的更新，关闭 WebUI 热重载时也会跳过，以便保留现有的验证和回退机制。

更新器优先使用匹配该提交的预构建网页，缺少时用包内固定的 Node.js 和候选版本的前端锁文件在临时目录构建。新版需要不同的 Python 依赖，或者候选控制台未能启动时，保留已安装版本。其他分支需要保留独立版更新支持，并兼容已安装的 Python 依赖。提交历史按需从所选 Git 源下载并缓存，已缓存的提交可在离线时分页查看。

新版客户端直接读 GitHub `dev` 及其 GitCode 镜像。`independent-windows` 分支仍可发布同一提交的 `release.json` 和 `frontend.zip`，供新版使用预构建网页，也能把旧版迁移到按 Git 更新的版本。旧版迁移仍用原来的发布密钥生成兼容清单。
