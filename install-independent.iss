#define PackageRoot GetEnv("AZURPILOT_PACKAGE_ROOT")
#define OutputRoot GetEnv("AZURPILOT_PACKAGE_OUTPUT")
#define VCRedist GetEnv("AZURPILOT_VC_REDIST")

[Setup]
AppId=AzurPilot.BeatriceBetty.Independent
AppName=AzurPilot 独立版
AppVersion=1.0.0
AppPublisher=Beatrice-betty
DefaultDirName={localappdata}\AzurPilotIndependent
DefaultGroupName=AzurPilot 独立版
DisableProgramGroupPage=yes
OutputDir={#OutputRoot}
OutputBaseFilename=AzurPilot_Independent_Windows_x64
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
PrivilegesRequired=admin
PrivilegesRequiredOverridesAllowed=commandline
Compression=zip/1
SolidCompression=no
WizardStyle=modern
UninstallDisplayIcon={app}\AzurPilot.exe
LicenseFile={#PackageRoot}\LICENSE
CloseApplications=no
#ifdef CodeSigning
SignTool=azurpilot
SignedUninstaller=yes
DiskSpanning=yes
DiskSliceSize=max
#endif

[Languages]
Name: "chinesesimplified"; MessagesFile: "compiler:Languages\ChineseSimplified.isl"

[Dirs]
Name: "{app}"; Flags: uninsneveruninstall

[Files]
Source: "{#PackageRoot}\*"; DestDir: "{app}"; Excludes: "\config\deploy.yaml,\log\*,\AzurPilot.exe,__pycache__\*"; Flags: ignoreversion recursesubdirs createallsubdirs
#ifdef CodeSigning
Source: "{#SignedLauncher}"; DestDir: "{app}"; Flags: replacesameversion
#else
Source: "{#PackageRoot}\AzurPilot.exe"; DestDir: "{app}"; Flags: ignoreversion
#endif
Source: "{#PackageRoot}\config\deploy.yaml"; DestDir: "{app}\config"; Flags: onlyifdoesntexist uninsneveruninstall
Source: "{#VCRedist}"; DestName: "azurpilot-vc-x64.exe"; Flags: dontcopy

[Icons]
Name: "{group}\AzurPilot 独立版"; Filename: "{app}\AzurPilot.exe"; WorkingDir: "{app}"

[Run]
Filename: "{sys}\icacls.exe"; Parameters: """{app}\runtime\webview2"" /grant ""*S-1-15-2-1:(OI)(CI)(RX)"" ""*S-1-15-2-2:(OI)(CI)(RX)"" /T /Q"; Flags: waituntilterminated runhidden
Filename: "{app}\AzurPilot.exe"; Description: "启动 AzurPilot 独立版"; Flags: postinstall nowait skipifsilent

[Code]
#include "install-independent-vc.iss.inc"

function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  ExitCode: Integer;
begin
  Result := '';
  if not MissingVCRuntime then
    Exit;
  if not IsAdminInstallMode then
  begin
    Result := '本机需要更新 Microsoft VC++ x64 运行库。请用默认安装方式运行完整安装包。';
    Exit;
  end;
  try
    ExtractTemporaryFile('azurpilot-vc-x64.exe');
    if not Exec(ExpandConstant('{tmp}\azurpilot-vc-x64.exe'), '/install /quiet /norestart',
        ExpandConstant('{tmp}'), SW_HIDE, ewWaitUntilTerminated, ExitCode) then
      Result := '无法启动 Microsoft VC++ 运行库安装程序。'
    else if (ExitCode = 3010) or (ExitCode = 1641) then
    begin
      NeedsRestart := True;
      Result := '运行库已安装。请重启 Windows 后重新运行安装程序。';
    end
    else if MissingVCRuntime then
      Result := 'Microsoft VC++ 运行库安装失败（' + IntToStr(ExitCode) + '）。请检查系统后重试。';
  except
    Result := '无法准备 Microsoft VC++ 运行库。' + GetExceptionMessage;
  end;
end;
