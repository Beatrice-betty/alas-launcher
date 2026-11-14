[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [ValidateSet('Full', 'Lite')]
    [string]$Edition,
    [Parameter(Mandatory)]
    [string]$PackageRoot,
    [Parameter(Mandatory)]
    [string]$OutputRoot,
    [Parameter(Mandatory)]
    [string]$ISCC,
    [string]$Launcher,
    [string]$VCRedist,
    [string]$BootstrapRoot,
    [switch]$UseLocalCertificate
)

$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $false
if (-not $IsWindows) { throw 'Run this script with PowerShell 7 on Windows.' }

$source = (Resolve-Path -LiteralPath $PackageRoot).Path
if (-not $Launcher) { $Launcher = Join-Path $source 'AzurPilot.exe' }
$launcherPath = (Resolve-Path -LiteralPath $Launcher).Path
$compiler = (Resolve-Path -LiteralPath $ISCC).Path
$destination = [IO.Path]::GetFullPath($OutputRoot)
$repo = Split-Path -Parent $PSScriptRoot
$signer = (Resolve-Path -LiteralPath (Join-Path $repo 'tools/ci/sign-windows.ps1')).Path
$pwshPath = (Get-Process -Id $PID).Path
if ($destination.Equals($source, [StringComparison]::OrdinalIgnoreCase) -or
    $destination.StartsWith($source.TrimEnd('\') + '\', [StringComparison]::OrdinalIgnoreCase)) {
    throw 'The output directory must be outside the input package.'
}
if (Test-Path -LiteralPath $destination) { throw 'Use a fresh output directory for each signing build.' }
& python -B -c "import sys; sys.path.insert(0, sys.argv[1]); from build_independent import check_launcher_paths; from pathlib import Path; check_launcher_paths(Path(sys.argv[2]))" $PSScriptRoot $launcherPath
if ($LASTEXITCODE) { throw 'The launcher contains local build paths or the privacy check could not run.' }

$variables = @(
    'WINDOWS_SIGNING_PFX_B64', 'WINDOWS_SIGNING_PASSWORD', 'WINDOWS_SIGNING_ALLOW_SELF_SIGNED',
    'AZURPILOT_LITE_ROOT', 'AZURPILOT_LITE_OUTPUT',
    'AZURPILOT_PACKAGE_ROOT', 'AZURPILOT_PACKAGE_OUTPUT', 'AZURPILOT_VC_REDIST',
    'AZURPILOT_FULL_BOOTSTRAP'
)
$previous = @{}
foreach ($name in $variables) { $previous[$name] = [Environment]::GetEnvironmentVariable($name) }

$keyBytes = $null
try {
    if ($UseLocalCertificate) {
        $directory = Join-Path ([Environment]::GetFolderPath('UserProfile')) '.azurpilot\code-signing'
        $keyBytes = [IO.File]::ReadAllBytes((Join-Path $directory 'AzurPilot-CodeSigning.pfx'))
        $password = ConvertTo-SecureString (Get-Content -LiteralPath (Join-Path $directory 'password.dpapi') -Raw).Trim()
        $env:WINDOWS_SIGNING_PFX_B64 = [Convert]::ToBase64String($keyBytes)
        $env:WINDOWS_SIGNING_PASSWORD = [Net.NetworkCredential]::new('', $password).Password
        $env:WINDOWS_SIGNING_ALLOW_SELF_SIGNED = 'true'
    }

    New-Item -ItemType Directory -Path $destination | Out-Null
    $signedLauncher = Join-Path $destination 'AzurPilot.exe'
    Copy-Item -LiteralPath $launcherPath -Destination $signedLauncher
    & $signer -Path $signedLauncher

    if ($Edition -eq 'Lite') {
        $script = Join-Path $repo 'install-independent-lite.iss'
        $env:AZURPILOT_LITE_ROOT = $source
        $env:AZURPILOT_LITE_OUTPUT = $destination
    } else {
        if (-not $VCRedist) { throw 'The full installer needs -VCRedist.' }
        if (-not $BootstrapRoot) { throw 'The full installer needs -BootstrapRoot (the lite bootstrap folder).' }
        $script = Join-Path $repo 'install-independent.iss'
        $env:AZURPILOT_PACKAGE_ROOT = $source
        $env:AZURPILOT_PACKAGE_OUTPUT = $destination
        $env:AZURPILOT_VC_REDIST = (Resolve-Path -LiteralPath $VCRedist).Path
        $env:AZURPILOT_FULL_BOOTSTRAP = (Resolve-Path -LiteralPath $BootstrapRoot).Path
    }
    $signCommand = '$q' + $pwshPath + '$q -NoProfile -NonInteractive -File $q' + $signer + '$q -Path $f'
    & $compiler /Qp '/DCodeSigning' "/DSignedLauncher=$signedLauncher" "/Sazurpilot=$signCommand" $script
    if ($LASTEXITCODE) { throw 'Inno Setup compilation or code signing failed.' }
} finally {
    foreach ($name in $variables) { [Environment]::SetEnvironmentVariable($name, $previous[$name]) }
    if ($keyBytes) { [Array]::Clear($keyBytes, 0, $keyBytes.Length) }
}
