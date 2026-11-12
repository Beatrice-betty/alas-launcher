# AzurPilot 独立版 Windows x64

安装到新的目录，打开「AzurPilot 独立版」即可。安装包带 Python、ADB、Git 和固定版 WebView2。控制台默认只在本机的 25549 端口打开。启动器和运行环境通过完整安装包升级。

用过旧版时，先退出旧启动器并备份原目录。在独立版开过一次后，再迁移实例配置和统计。独立版的 `config/deploy.yaml` 指向包内工具，应保留，旧版的部署配置和程序文件请留在旧目录。迁移实例时也需要原来的账号保险库和相关配置，是否可直接迁移取决于原安装使用的版本和存储方式。

## 构建完整安装包

1. 准备自有 AzurPilot 仓库已提交的 `dev` 版本。用该版本的前端锁文件构建 `frontend/dist`，为 Windows x64 准备锁定的 Python 解释器和生产依赖。
2. 准备独立启动器的前端，使用 `npm ci --prefix frontend` 和 `npm run build --prefix frontend`。PowerShell 设置 `$env:TAURI_CONFIG = '{"identifier":"io.github.beatrice-betty.azurpilot.independent"}'` 后运行 `cargo build --locked --release --features independent`。
3. 把 Python 生产依赖放在输入目录的 `python-build/Lib/site-packages`。同一输入目录还需要 `mingit`、`adb/platform-tools`、`atx` 和 `webview2/<固定版本目录>`。ATX 使用 openatx v0.10.0 的官方文件，程序会核对摘要。VC++ x64 运行库用微软发行文件。
4. 运行 `tools/build_independent.py --alas <ALAS源码目录> --release <新发布目录> --key <本机发布密钥文件> --package <新装配目录> --inputs <组件输入目录> --python-root <Python解释器目录> --launcher <已编译EXE>`。发布密钥需要保存在仓库及打包目录之外。
5. 为 Inno Setup 设置 `AZURPILOT_PACKAGE_ROOT`、`AZURPILOT_PACKAGE_OUTPUT` 和 `AZURPILOT_VC_REDIST`，分别指向装配目录、输出目录和 VC++ x64 安装文件。运行 `ISCC.exe install-independent.iss`，输出完整安装程序。

## 发布仅更新 ALAS 的版本

每次发布使用审核过、已推送到 GitHub 的 ALAS 提交，重新构建网页。用同一个本机发布密钥运行步骤 4，省略 `--package`、`--inputs`、`--python-root` 和 `--launcher`。把生成的 `release.json` 和 `frontend.zip` 一起提交到自有 GitHub 仓库的 `independent-windows` 分支。GitHub Actions 负责把该分支和 `dev` 同步给 GitCode。

独立包优先查询 GitCode，遇到旧清单、连接失败或校验错误时再查询 GitHub。更新会核对签名、网页摘要和 ALAS 提交。依赖清单变化时，用户继续使用已安装版本，需要更新时安装新完整包。应用更新需要可用的磁盘空间来下载和暂存源码，控制台会在暂存验证后才切换。

只向 GitHub 提交 `dev` 会同步源码。安装包看到的是已发布的签名版本，发布步骤需要执行一次才能向已安装的客户端提供更新。
