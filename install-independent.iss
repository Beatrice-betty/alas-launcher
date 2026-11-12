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

[Languages]
Name: "chinesesimplified"; MessagesFile: "compiler:Languages\ChineseSimplified.isl"

[Dirs]
Name: "{app}"; Flags: uninsneveruninstall

[Files]
Source: "{#PackageRoot}\*"; DestDir: "{app}"; Excludes: "\config\deploy.yaml,\log\*"; Flags: ignoreversion recursesubdirs createallsubdirs
Source: "{#PackageRoot}\config\deploy.yaml"; DestDir: "{app}\config"; Flags: onlyifdoesntexist uninsneveruninstall
Source: "{#VCRedist}"; DestDir: "{tmp}"; DestName: "azurpilot-vc-x64.exe"; Flags: deleteafterinstall

[Icons]
Name: "{group}\AzurPilot 独立版"; Filename: "{app}\AzurPilot.exe"; WorkingDir: "{app}"

[Run]
Filename: "{tmp}\azurpilot-vc-x64.exe"; Parameters: "/install /quiet /norestart"; Flags: waituntilterminated runhidden; Check: MissingVCRuntime
Filename: "{sys}\icacls.exe"; Parameters: """{app}\runtime\webview2"" /grant ""*S-1-15-2-1:(OI)(CI)(RX)"" ""*S-1-15-2-2:(OI)(CI)(RX)"" /T /Q"; Flags: waituntilterminated runhidden
Filename: "{app}\AzurPilot.exe"; Description: "启动 AzurPilot 独立版"; Flags: postinstall nowait skipifsilent

[Code]
function MissingVCRuntime: Boolean;
var
  Installed: Cardinal;
begin
  Result := IsAdminInstallMode;
  if not Result then
    Exit;
  if RegQueryDWordValue(HKLM64, 'SOFTWARE\Microsoft\VisualStudio\14.0\VC\Runtimes\x64', 'Installed', Installed) then
    Result := Installed <> 1;
end;
