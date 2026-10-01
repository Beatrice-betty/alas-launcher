param(
    [Parameter(Mandatory)][string]$AlasSource,
    [Parameter(Mandatory)][string]$BootstrapRoot,
    [Parameter(Mandatory)][string]$LauncherExecutable,
    [Parameter(Mandatory)][string]$InnoCompiler,
    [Parameter(Mandatory)][string]$SetupRoot,
    [Parameter(Mandatory)][string]$CrtRoot,
    [Parameter(Mandatory)][string]$OutputDirectory,
    [string]$Repository = 'https://github.com/Beatrice-betty/AzurPilot.git',
    [string]$Branch = 'codex/lifecycle-diagnostics'
)

$ErrorActionPreference = 'Stop'
$sourceRoot = Split-Path $PSScriptRoot -Parent
$versionMatch = [regex]::Match([IO.File]::ReadAllText("$sourceRoot\Cargo.toml"), '(?m)^version = "([^"]+)"')
if (-not $versionMatch.Success) { throw '无法读取启动器版本' }
$version = $versionMatch.Groups[1].Value
foreach ($required in @("$AlasSource\deploy\installer.py", "$BootstrapRoot\adb.exe", "$BootstrapRoot\AdbWinApi.dll", "$BootstrapRoot\AdbWinUsbApi.dll", "$BootstrapRoot\git\cmd\git.exe", "$CrtRoot\vcruntime140.dll", "$CrtRoot\vcruntime140_1.dll", "$SetupRoot\vcredist_x64.exe", $LauncherExecutable, $InnoCompiler)) {
    if (-not (Test-Path -LiteralPath $required -PathType Leaf)) { throw "缺少打包输入：$required" }
}
if ((Get-Item -LiteralPath "$SetupRoot\vcredist_x64.exe").VersionInfo.FileMajorPart -lt 14) {
    throw '安装器需要 VC++ v14，不能使用旧的 VC++ 2013 运行库'
}
if (Test-Path -LiteralPath $OutputDirectory) { throw '输出目录已存在，请选择新的目录以保留旧产物' }
$OutputDirectory = [IO.Path]::GetFullPath($OutputDirectory)
$packageRoot = Join-Path $OutputDirectory 'AzurPilot-Diagnostics'
New-Item -ItemType Directory -Path "$packageRoot\config", "$packageRoot\bootstrap" -Force | Out-Null

# 只取 Git 中已提交的部署工具，不复制账号配置、缓存或工作区未提交文件。
git -C $AlasSource archive --format=zip "--output=$OutputDirectory\deploy-source.zip" HEAD deploy
if ($LASTEXITCODE -ne 0) { throw '导出本体部署工具失败' }
Expand-Archive -LiteralPath "$OutputDirectory\deploy-source.zip" -DestinationPath $packageRoot
Copy-Item -LiteralPath $LauncherExecutable -Destination "$packageRoot\alas-launcher.exe"
foreach ($name in @('vcruntime140.dll', 'vcruntime140_1.dll')) {
    Copy-Item -LiteralPath (Join-Path $CrtRoot $name) -Destination (Join-Path $packageRoot $name)
}
foreach ($name in @('adb.exe', 'AdbWinApi.dll', 'AdbWinUsbApi.dll')) {
    Copy-Item -LiteralPath (Join-Path $BootstrapRoot $name) -Destination "$packageRoot\bootstrap\$name"
}
Copy-Item -LiteralPath "$BootstrapRoot\git" -Destination "$packageRoot\bootstrap\git" -Recurse
$config = [IO.File]::ReadAllText("$sourceRoot\deploy.windows-cn.yaml")
$config = $config.Replace('Repository: git://git.pull/AzurPilot', "Repository: $Repository").Replace('Branch: master', "Branch: $Branch")
[IO.File]::WriteAllText("$packageRoot\config\deploy.yaml", $config, [Text.UTF8Encoding]::new($false))

# 诊断版使用独立安装身份和目录；不能沿用上游按名称清理所有 Python/Git 的操作。
$installer = [IO.File]::ReadAllText("$sourceRoot\install.iss")
$installer = $installer.Replace('AppName=AzurPilot', 'AppName=AzurPilot Diagnostics')
$installer = $installer.Replace('ArchitecturesAllowed=x86 x64', 'ArchitecturesAllowed=x64compatible')
$installer = $installer.Replace('ArchitecturesInstallIn64BitMode=x64', 'ArchitecturesInstallIn64BitMode=x64compatible')
$installer = $installer.Replace('; 应用本体', '; 应用本体' + "`r`n" + 'Source: "{#PackageRoot}\*.dll"; DestDir: "{app}"; Flags: ignoreversion; Permissions: users-modify')
$installer = $installer.Replace('DefaultDirName={autopf}\AzurPilot', 'DefaultDirName={autopf}\AzurPilot-Diagnostics')
$installer = $installer.Replace('DefaultGroupName=AzurPilot', 'DefaultGroupName=AzurPilot Diagnostics')
$installer = $installer.Replace('AppId={{1A779131-3DD5-067C-0ABC-E656396F6879}', 'AppId={{B0C9B25D-65C3-459E-A039-C754D7E244C0}')
$cleanupPattern = '(?s)(procedure StopRunningProcesses;\s*begin).*?(end;)'
if ([regex]::Matches($installer, $cleanupPattern).Count -ne 1) { throw '安装器清理入口已变化，需要重新审查打包规则' }
$installer = [regex]::Replace($installer, $cleanupPattern, '$1' + "`r`n  // 诊断包不结束其他安装目录或开发工具的进程。`r`n" + '$2')
$installerPath = "$OutputDirectory\install-diagnostics.iss"
[IO.File]::WriteAllText($installerPath, $installer, [Text.UTF8Encoding]::new($false))
$messages = Join-Path (Split-Path $InnoCompiler -Parent) 'Languages\ChineseSimplified.isl'
& $InnoCompiler $installerPath "/O$OutputDirectory" "/DAppVersion=$version" "/DOutputBaseFilename=AzurPilot_Diagnostics_Setup_$version" "/DPackageRoot=$packageRoot" "/DSetupRoot=$SetupRoot" "/DChineseMessagesFile=$messages"
if ($LASTEXITCODE -ne 0) { throw '安装 EXE 编译失败' }

$readme = @"
AzurPilot $version 生命周期诊断版

安装 EXE：安装到默认的独立 AzurPilot-Diagnostics 目录。
便携 ZIP：完整解压后运行目录中的 alas-launcher.exe，不要只取一个 EXE。
首次启动需联网准备 Python、依赖和本体；本体从 $Repository 的 $Branch 分支获取。
开始诊断前，请先完整退出旧实例，再迁移需要的实例配置；不要让两个实例同时控制同一模拟器。
本包未包含个人配置、发行用 mTLS 证书，也未安装或替换现有 AzurPilot。
诊断日志位于安装目录的 log/diagnostics/，复现后先保留整套日志再重启。
"@
[IO.File]::WriteAllText("$packageRoot\使用说明.txt", $readme, [Text.UTF8Encoding]::new($false))
Compress-Archive -LiteralPath $packageRoot -DestinationPath "$OutputDirectory\AzurPilot_Diagnostics_Portable_$version.zip"
$provenance = [ordered]@{
    version = $version
    launcher_commit = (git -C $sourceRoot rev-parse HEAD)
    alas_commit = (git -C $AlasSource rev-parse HEAD)
    repository = $Repository
    branch = $Branch
    launcher_sha256 = (Get-FileHash -LiteralPath "$packageRoot\alas-launcher.exe" -Algorithm SHA256).Hash
    artifacts = @(Get-ChildItem -LiteralPath $OutputDirectory -File | Where-Object { $_.Extension -in '.exe', '.zip' -and $_.Name -ne 'deploy-source.zip' } | ForEach-Object {
        [ordered]@{ name = $_.Name; bytes = $_.Length; sha256 = (Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash }
    })
}
[IO.File]::WriteAllText("$OutputDirectory\build-info.json", ($provenance | ConvertTo-Json -Depth 5), [Text.UTF8Encoding]::new($false))
Write-Output "打包完成：$OutputDirectory"
