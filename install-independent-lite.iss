#define PackageRoot GetEnv("AZURPILOT_LITE_ROOT")
#define OutputRoot GetEnv("AZURPILOT_LITE_OUTPUT")

[Setup]
AppId=AzurPilot.BeatriceBetty.Independent
AppName=AzurPilot 独立版
AppVersion=1.0.0
AppVerName=AzurPilot 独立版 1.0.0（简装）
AppPublisher=Beatrice-betty
DefaultDirName={localappdata}\AzurPilotIndependent
DefaultGroupName=AzurPilot 独立版
DisableProgramGroupPage=yes
OutputDir={#OutputRoot}
OutputBaseFilename=AzurPilot_Independent_Lite_Windows_x64
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0.17134
PrivilegesRequired=admin
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
Source: "{#PackageRoot}\bootstrap\*"; DestDir: "{tmp}\azurpilot-lite"; Flags: dontcopy noencryption recursesubdirs
Source: "{tmp}\azurpilot-lite\ready\*"; DestDir: "{app}"; Excludes: "\config\deploy.yaml"; ExternalSize: 2200000000; Flags: external ignoreversion recursesubdirs createallsubdirs; Check: NeedsProvision
Source: "{tmp}\azurpilot-lite\ready\config\deploy.yaml"; DestDir: "{app}\config"; ExternalSize: 4096; Flags: external onlyifdoesntexist uninsneveruninstall; Check: NeedsProvision
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
Filename: "{sys}\icacls.exe"; Parameters: """{app}\runtime\webview2"" /grant ""*S-1-15-2-1:(OI)(CI)(RX)"" ""*S-1-15-2-2:(OI)(CI)(RX)"" /T /Q"; Flags: waituntilterminated runhidden; Check: NeedsProvision
Filename: "{app}\AzurPilot.exe"; Description: "启动 AzurPilot 独立版"; Flags: postinstall nowait skipifsilent

[Code]
var
  Provision: Boolean;
  Progress: TOutputProgressWizardPage;

function NeedsProvision: Boolean;
begin
  Result := Provision;
end;

function ReadyInstallation(const Dir: String): Boolean;
begin
  Result :=
    FileExists(Dir + '\.azurpilot\independent.json') and
    FileExists(Dir + '\.azurpilot\independent-files.json') and
    FileExists(Dir + '\runtime\independent_update.py') and
    FileExists(Dir + '\runtime\python\python.exe') and
    FileExists(Dir + '\runtime\webview2\msedgewebview2.exe') and
    FileExists(Dir + '\runtime\git\cmd\git.exe') and
    FileExists(Dir + '\runtime\adb\adb.exe') and
    FileExists(Dir + '\frontend\dist\index.html') and
    FileExists(Dir + '\gui.py');
end;

function DirectoryHasFiles(const Dir, ExceptName: String): Boolean;
var
  Item: TFindRec;
begin
  Result := False;
  if not FindFirst(Dir + '\*', Item) then
    Exit;
  try
    repeat
      if (Item.Name <> '.') and (Item.Name <> '..') and (CompareText(Item.Name, ExceptName) <> 0) then
      begin
        Result := True;
        Exit;
      end;
    until not FindNext(Item);
  finally
    FindClose(Item);
  end;
end;

#include "install-independent-vc.iss.inc"

procedure InitializeWizard;
begin
  Progress := CreateOutputProgressPage('准备 AzurPilot 独立版', '首次安装需要联网下载锁定的运行环境与 ALAS。');
end;

procedure ProvisionOutput(const Line: String; const Error, FirstLine: Boolean);
var
  Step: Integer;
  LabelText: String;
begin
  Log(Line);
  if Error then
    Exit;
  if Line = 'STEP VC' then
  begin
    Progress.SetText('准备 Microsoft VC++ 运行库', '请保持安装窗口打开。');
    Exit;
  end;
  if Copy(Line, 1, 11) = 'DOWNLOADED ' then
  begin
    Progress.SetText('下载并校验组件', '已就绪 ' + Copy(Line, 12, Length(Line)));
    Exit;
  end;
  if Copy(Line, 1, 5) <> 'STEP ' then
    Exit;
  Step := StrToIntDef(Copy(Line, 6, Length(Line)), 0);
  case Step of
    2: LabelText := '下载并校验组件';
    3: LabelText := '展开固定版 WebView2';
    4: LabelText := '安装锁定的 Python 依赖';
    5: LabelText := '下载 ALAS 程序与资源';
    6: LabelText := '检查本机控制台是否能启动';
    7: LabelText := '运行环境已经准备好';
  else
    LabelText := '准备首次安装';
  end;
  Progress.SetText(LabelText, '请保持安装窗口打开。');
  Progress.SetProgress(Step, 7);
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  Dir, ToolDir, Params: String;
  ExitCode: Integer;
  NeedVC: Boolean;
begin
  Result := '';
  Dir := ExpandConstant('{app}');
  Provision := not ReadyInstallation(Dir);
  NeedVC := MissingVCRuntime;
  if not Provision and not NeedVC then
    Exit;
  if Provision and (DirectoryHasFiles(Dir, '.azurpilot') or
      DirectoryHasFiles(Dir + '\.azurpilot', 'lite-setup')) then
  begin
    Result := '这个目录有原来的安装或数据。请选择空目录，或者选用完整包修复已有独立版。';
    Exit;
  end;

  if NeedVC and not IsAdminInstallMode then
  begin
    Result := '本机需要安装或更新 Microsoft VC++ x64 运行库。请用默认安装方式重新打开简装包。';
    Exit;
  end;
  Progress.SetText('准备首次安装', '请保持安装窗口打开。');
  Progress.SetProgress(1, 7);
  Progress.Show;
  try
    try
      ExtractTemporaryFiles('{tmp}\azurpilot-lite\*');
      ToolDir := ExpandConstant('{tmp}\azurpilot-lite');
      Params := '-I -B "' + ToolDir + '\bootstrap.py" ';
      if Provision then
      begin
        Params := Params + 'prepare "' + Dir + '"';
        if NeedVC then
          Params := Params + ' --install-vc';
      end
      else
        Params := Params + 'install-vc "' + Dir + '"';
      if not ExecAndLogOutput(ToolDir + '\python\python.exe', Params, ToolDir, SW_SHOWNORMAL,
          ewWaitUntilTerminated, ExitCode, @ProvisionOutput) then
        Result := '无法运行包内的安装程序。'
      else if ExitCode = 3010 then
      begin
        NeedsRestart := True;
        Result := '运行库已安装。请重启 Windows 后再运行简装包。';
      end
      else if ExitCode <> 0 then
        Result := '首次安装未完成。可以重试，已校验的下载会继续使用。详情见 ' +
          Dir + '\.azurpilot\lite-setup\error.txt'
      else if NeedVC and MissingVCRuntime then
        Result := 'Microsoft VC++ x64 运行库版本仍未达到要求。请检查系统后重试。';
    except
      Result := '准备运行环境失败。' + GetExceptionMessage;
    end;
  finally
    Progress.Hide;
  end;
end;

procedure CurStepChanged(CurStep: TSetupStep);
var
  ExitCode: Integer;
  ToolDir: String;
begin
  if (CurStep <> ssPostInstall) or not Provision or not ReadyInstallation(ExpandConstant('{app}')) then
    Exit;
  ToolDir := ExpandConstant('{tmp}\azurpilot-lite');
  ExecAndLogOutput(ToolDir + '\python\python.exe',
    '-I -B "' + ToolDir + '\bootstrap.py" clean "' + ExpandConstant('{app}') + '"',
    ToolDir, SW_SHOWNORMAL, ewWaitUntilTerminated, ExitCode, nil);
end;
