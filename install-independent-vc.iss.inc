#define MinimumVCRuntime "14.51.36247.0"

function VCRuntimeUsableInView(RootKey: Integer): Boolean;
var
  Installed: Cardinal;
  Version, Key: String;
  CurrentVersion, RequiredVersion: Int64;
begin
  Result := False;
  Key := 'SOFTWARE\Microsoft\VisualStudio\14.0\VC\Runtimes\x64';
  if not RegQueryDWordValue(RootKey, Key, 'Installed', Installed) or (Installed <> 1) then
    Exit;
  if not RegQueryStringValue(RootKey, Key, 'Version', Version) then
    Exit;
  if CompareText(Copy(Version, 1, 1), 'v') = 0 then
    Delete(Version, 1, 1);
  if StrToVersion(Version, CurrentVersion) and StrToVersion('{#MinimumVCRuntime}', RequiredVersion) then
    Result := ComparePackedVersion(CurrentVersion, RequiredVersion) >= 0;
end;

function MissingVCRuntime: Boolean;
begin
  Result := not (VCRuntimeUsableInView(HKLM64) or VCRuntimeUsableInView(HKLM32));
end;
