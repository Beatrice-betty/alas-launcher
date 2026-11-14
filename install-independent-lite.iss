#define PackageRoot GetEnv("AZURPILOT_LITE_ROOT")
#define OutputRoot GetEnv("AZURPILOT_LITE_OUTPUT")

[Setup]
AppId=AzurPilot.BeatriceBetty.Independent
AppName=AzurPilot 独立版
AppVersion=1.0.1
AppVerName=AzurPilot 独立版 1.0.1（简装）
AppPublisher=Beatrice-betty
DefaultDirName={localappdata}\AzurPilotIndependent
DefaultGroupName=AzurPilot 独立版
DisableProgramGroupPage=yes
OutputDir={#OutputRoot}
OutputBaseFilename=AzurPilot_Independent_Lite_Windows_x64
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0.17134
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=commandline
Compression=lzma2/fast
SolidCompression=no
WizardStyle=modern
UninstallDisplayIcon={app}\AzurPilot.exe
LicenseFile={#PackageRoot}\LICENSE
CloseApplications=no
#ifdef CodeSigning
SignTool=azurpilot
SignedUninstaller=yes
#endif
SetupMutex=AzurPilotIndependentLiteSetup

[Languages]
Name: "chinesesimplified"; MessagesFile: "compiler:Languages\ChineseSimplified.isl"

[Dirs]
Name: "{app}"; Flags: uninsneveruninstall

[Files]
Source: "{#PackageRoot}\bootstrap\*"; DestDir: "{app}\.azurpilot\bootstrap"; Flags: ignoreversion recursesubdirs
Source: "{#PackageRoot}\bootstrap\deploy.yaml"; DestDir: "{app}\config"; Flags: onlyifdoesntexist uninsneveruninstall
Source: "{#PackageRoot}\bootstrap\python\vcruntime140*.dll"; DestDir: "{app}"; Flags: onlyifdoesntexist
#ifdef CodeSigning
Source: "{#SignedLauncher}"; DestDir: "{app}"; Flags: replacesameversion
#else
Source: "{#PackageRoot}\AzurPilot.exe"; DestDir: "{app}"; Flags: onlyifdoesntexist
#endif
Source: "{#PackageRoot}\LICENSE"; DestDir: "{app}"; Flags: onlyifdoesntexist
Source: "{#PackageRoot}\launcher-LICENSE.txt"; DestDir: "{app}"; Flags: onlyifdoesntexist
Source: "{#PackageRoot}\使用说明.txt"; DestDir: "{app}"; Flags: onlyifdoesntexist

[Icons]
Name: "{group}\AzurPilot 独立版"; Filename: "{app}\AzurPilot.exe"; WorkingDir: "{app}"

[Run]
Filename: "{app}\AzurPilot.exe"; Description: "启动 AzurPilot 独立版"; Flags: postinstall nowait skipifsilent
