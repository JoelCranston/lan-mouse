; Inno Setup script for the Lan Mouse Windows installer.
;
; Stage the files first with scripts\package-windows.ps1, then:
;   iscc /DAppVersion=0.11.0 build-aux\windows\lan-mouse.iss
; The installer is written to target\.

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef SourceDir
  #define SourceDir "..\..\target\lan-mouse-windows"
#endif
#ifndef OutputDir
  #define OutputDir "..\..\target"
#endif

#define AppName "Lan Mouse"
#define AppExe "lan-mouse.exe"
#define RunKey "Software\Microsoft\Windows\CurrentVersion\Run"

[Setup]
; Never change AppId: it ties upgrades and uninstall to earlier installs.
AppId={{75AE66D0-EE51-418E-A3EA-480071070610}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisherURL=https://github.com/feschber/lan-mouse
AppSupportURL=https://github.com/feschber/lan-mouse/issues
DefaultDirName={autopf}\{#AppName}
DisableProgramGroupPage=yes
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
PrivilegesRequired=admin
LicenseFile=..\..\LICENSE
OutputDir={#OutputDir}
OutputBaseFilename=lan-mouse-windows-x86_64-setup
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
UninstallDisplayIcon={app}\{#AppExe}
UninstallDisplayName={#AppName}
; Lan Mouse hides to the tray instead of closing, so the Restart Manager
; can't shut it down; PrepareToInstall stops it instead.
CloseApplications=no

[Tasks]
Name: "desktopicon"; Description: "Create a &desktop shortcut"; Flags: unchecked
Name: "autostart"; Description: "&Start Lan Mouse in the notification area when I sign in"; Flags: unchecked
Name: "firewall"; Description: "Allow Lan Mouse through Windows &Firewall on private networks"

[Files]
Source: "{#SourceDir}\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{autoprograms}\{#AppName}"; Filename: "{app}\{#AppExe}"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExe}"; Tasks: desktopicon

[Registry]
; Same value the in-app "Start with Windows" switch writes.
Root: HKCU; Subkey: "{#RunKey}"; ValueType: string; ValueName: "{#AppName}"; \
  ValueData: """{app}\{#AppExe}"" --hidden"; Tasks: autostart

[Run]
; Replace any earlier rule (from a previous install) rather than adding a duplicate.
Filename: "{sys}\netsh.exe"; \
  Parameters: "advfirewall firewall delete rule name=""{#AppName}"""; \
  Flags: runhidden; Tasks: firewall
Filename: "{sys}\netsh.exe"; \
  Parameters: "advfirewall firewall add rule name=""{#AppName}"" dir=in action=allow program=""{app}\{#AppExe}"" enable=yes profile=private"; \
  Flags: runhidden; Tasks: firewall
Filename: "{app}\{#AppExe}"; Description: "Launch {#AppName}"; \
  Flags: nowait postinstall skipifsilent runasoriginaluser

[UninstallRun]
Filename: "{sys}\taskkill.exe"; Parameters: "/F /IM {#AppExe}"; Flags: runhidden; RunOnceId: "StopLanMouse"
Filename: "{sys}\netsh.exe"; \
  Parameters: "advfirewall firewall delete rule name=""{#AppName}"""; \
  Flags: runhidden; RunOnceId: "DeleteFirewallRule"

[Code]
procedure StopLanMouse();
var
  ResultCode: Integer;
begin
  // Stops both the GUI and its daemon child; harmless if neither is running.
  Exec(ExpandConstant('{sys}\taskkill.exe'), '/F /IM {#AppExe}', '',
    SW_HIDE, ewWaitUntilTerminated, ResultCode);
  Sleep(500);
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  StopLanMouse();
  Result := '';
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  // The in-app switch may have added the autostart entry even if the
  // installer task was off, so always remove it. Config and certificates in
  // %LOCALAPPDATA%\lan-mouse are kept.
  if CurUninstallStep = usPostUninstall then
    RegDeleteValue(HKEY_CURRENT_USER, '{#RunKey}', '{#AppName}');
end;
