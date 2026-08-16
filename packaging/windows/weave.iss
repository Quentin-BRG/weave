; Inno Setup script for WeaveSetup-x64.exe.
;
; Inno Setup because Weave is a single CLI executable plus a bundled helper:
; MSI/WiX would model a component database Weave does not have, and Inno is the
; only mature Windows installer that installs per-user without ever asking for
; administrator rights, which is what lets `weave` land in %LOCALAPPDATA% and
; still appear in Settings > Installed apps.
;
; Installed layout:
;
;     %LOCALAPPDATA%\Programs\Weave\weave.exe
;     %LOCALAPPDATA%\Programs\Weave\cloudflared.exe
;     %LOCALAPPDATA%\Programs\Weave\weave-bundle.json
;     %LOCALAPPDATA%\Programs\Weave\weave.ico
;     %LOCALAPPDATA%\Programs\Weave\LICENSE.txt
;     %LOCALAPPDATA%\Programs\Weave\licenses\cloudflared\{LICENSE,NOTICE}
;
; `weave.exe` finds `cloudflared.exe` beside itself (see src/install.rs), so the
; flat layout is load-bearing rather than cosmetic.
;
; THIS INSTALLER IS NOT CODE-SIGNED. No signing certificate is referenced and no
; secret is required to build it. SmartScreen will warn until the download earns
; reputation; see the README.
;
; Built by packaging/windows/build.ps1, which passes AppVersion,
; AppFileVersion, StageDir, RepoDir and OutputDir.

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
; The Win32 VERSIONINFO resource is four numbers; a semver prerelease such as
; `0.1.0-rc.1` is not a legal value and aborts the compile. build.ps1 derives
; the numeric part, and only this field uses it.
#ifndef AppFileVersion
  #define AppFileVersion "0.0.0"
#endif
#ifndef StageDir
  #define StageDir "stage"
#endif
#ifndef RepoDir
  #define RepoDir ".."
#endif
#ifndef OutputDir
  #define OutputDir "dist"
#endif

[Setup]
; Never regenerate this GUID: it is what makes an upgrade an upgrade rather
; than a second copy in Installed apps.
AppId={{6C5E5B1A-6E3B-4C7C-9E1B-6F1E3B5D0A21}
AppName=Weave
AppVersion={#AppVersion}
AppVerName=Weave {#AppVersion}
AppPublisher=Weave
AppPublisherURL=https://github.com/Quentin-BRG/weave
AppSupportURL=https://github.com/Quentin-BRG/weave/issues
AppUpdatesURL=https://github.com/Quentin-BRG/weave/releases
VersionInfoVersion={#AppFileVersion}
VersionInfoTextVersion={#AppVersion}

; Per-user installation: no UAC prompt, no administrator, no elevation dialog
; on a locked-down machine.
PrivilegesRequired=lowest
DefaultDirName={localappdata}\Programs\Weave
DisableProgramGroupPage=yes
DisableDirPage=auto
DisableReadyPage=no
UsePreviousAppDir=yes

; Weave is a CLI: no Start menu clutter, no desktop icon, no file associations.
Uninstallable=yes
UninstallDisplayName=Weave
UninstallDisplayIcon={app}\weave.ico

; The installer writes HKCU\Environment\Path, so Inno broadcasts
; WM_SETTINGCHANGE and new terminals see `weave` without a sign-out.
ChangesEnvironment=yes

LicenseFile={#StageDir}\LICENSE.txt
SetupIconFile={#RepoDir}\assets\icons\windows\weave.ico
WizardImageFile={#RepoDir}\assets\icons\windows\wizard-large.bmp
WizardSmallImageFile={#RepoDir}\assets\icons\windows\wizard-small.bmp
WizardStyle=modern

OutputDir={#OutputDir}
OutputBaseFilename=WeaveSetup-x64
Compression=lzma2/max
SolidCompression=yes

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Files]
Source: "{#StageDir}\weave.exe";          DestDir: "{app}"; Flags: ignoreversion
Source: "{#StageDir}\cloudflared.exe";    DestDir: "{app}"; Flags: ignoreversion
Source: "{#StageDir}\weave-bundle.json";  DestDir: "{app}"; Flags: ignoreversion
Source: "{#StageDir}\weave.ico";          DestDir: "{app}"; Flags: ignoreversion
Source: "{#StageDir}\LICENSE.txt";        DestDir: "{app}"; Flags: ignoreversion
Source: "{#StageDir}\licenses\*";         DestDir: "{app}\licenses"; \
    Flags: ignoreversion recursesubdirs createallsubdirs

[Code]
const
  EnvironmentKey = 'Environment';

var
  SelfCheckFailed: Boolean;

function InitializeSetup(): Boolean;
begin
  Result := True;
  if not IsWin64 then
  begin
    MsgBox('Weave requires 64-bit Windows.' + #13#10 +
           'This installer contains only x64 binaries.',
           mbCriticalError, MB_OK);
    Result := False;
  end;
end;

{ ------------------------------------------------------------------------- }
{ The user's PATH                                                            }
{ ------------------------------------------------------------------------- }

function PathContains(const Paths, Value: string): Boolean;
begin
  Result := Pos(';' + Uppercase(Value) + ';', ';' + Uppercase(Paths) + ';') > 0;
end;

procedure EnvAddPath(const Value: string);
var
  Paths: string;
begin
  if not RegQueryStringValue(HKEY_CURRENT_USER, EnvironmentKey, 'Path', Paths) then
    Paths := '';
  if PathContains(Paths, Value) then
    exit;
  if Paths = '' then
    Paths := Value
  else
  begin
    if Paths[Length(Paths)] <> ';' then
      Paths := Paths + ';';
    Paths := Paths + Value;
  end;
  { REG_EXPAND_SZ: the rest of the user's PATH usually contains %VARIABLES%,
    and rewriting it as a plain string would flatten them permanently. }
  RegWriteExpandStringValue(HKEY_CURRENT_USER, EnvironmentKey, 'Path', Paths);
end;

procedure EnvRemovePath(const Value: string);
var
  Paths: string;
  Position: Integer;
begin
  if not RegQueryStringValue(HKEY_CURRENT_USER, EnvironmentKey, 'Path', Paths) then
    exit;
  { Searching the padded string is what lets the first and last entries match;
    Position is then the index of the separator *before* the value. }
  Position := Pos(';' + Uppercase(Value) + ';', ';' + Uppercase(Paths) + ';');
  if Position = 0 then
    exit;
  if Position = 1 then
    { First entry: remove it together with the separator that follows it. }
    Delete(Paths, 1, Length(Value) + 1)
  else
    { Anywhere else: remove the separator that precedes it, so neither a
      doubled nor a trailing ';' is left behind. }
    Delete(Paths, Position - 1, Length(Value) + 1);
  RegWriteExpandStringValue(HKEY_CURRENT_USER, EnvironmentKey, 'Path', Paths);
end;

{ ------------------------------------------------------------------------- }
{ Installation self-check                                                    }
{ ------------------------------------------------------------------------- }

{ `weave doctor --install` is the installation diagnostic: it needs no Git
  repository, so it is safe to run from an installer, and it is the same check
  the macOS and Debian packages run. The repository diagnostic is never run
  here — the installer has no idea which project the user cares about. }
procedure RunSelfCheck();
var
  LogPath, Command: string;
  ResultCode: Integer;
begin
  LogPath := ExpandConstant('{app}\install-check.log');
  Command := '/C ""' + ExpandConstant('{app}\weave.exe') + '" doctor --install > "' +
             LogPath + '" 2>&1"';
  if not Exec(ExpandConstant('{cmd}'), Command, '', SW_HIDE, ewWaitUntilTerminated, ResultCode) then
    ResultCode := -1;

  SelfCheckFailed := ResultCode <> 0;
  if SelfCheckFailed then
    SuppressibleMsgBox(
      'Weave installed, but its self-check failed.' + #13#10#13#10 +
      'Details were written to:' + #13#10 + LogPath + #13#10#13#10 +
      'Please report this at https://github.com/Quentin-BRG/weave/issues',
      mbCriticalError, MB_OK, IDOK);
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssPostInstall then
  begin
    EnvAddPath(ExpandConstant('{app}'));
    RunSelfCheck();
  end;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usUninstall then
    EnvRemovePath(ExpandConstant('{app}'));
end;

[UninstallDelete]
; Written after installation, so Inno does not track it.
Type: files; Name: "{app}\install-check.log"
