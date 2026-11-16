; CineFS Windows Installer Script (Inno Setup 6)
; Builds a single-click CineFS-Setup.exe that:
; 1. Installs CineFS.exe (GUI + System Tray) and cinefs-cli.exe
; 2. Silently installs the bundled WinFsp runtime MSI if not already present
; 3. Registers Windows Explorer right-click "Mount with CineFS" for .MLV, .mcraw, .braw, and folders
; 4. Optionally adds the virtual mount folder to Windows Defender exclusions for zero-latency 4K playback

#define MyAppName "CineFS"
#define MyAppVersion "0.1.0"
#define MyAppPublisher "Ãzcan Ãelik"
#define MyAppURL "https://github.com/ozcancelik/CineFS"
#define MyAppExeName "CineFS.exe"

[Setup]
AppId={{8D4F69C3-5E12-4B7A-9B0A-43494E454653}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppPublisher={#MyAppPublisher}
AppPublisherURL={#MyAppURL}
AppSupportURL={#MyAppURL}
AppUpdatesURL={#MyAppURL}
DefaultDirName={autopf}\{#MyAppName}
DefaultGroupName={#MyAppName}
AllowNoIcons=yes
LicenseFile=..\LICENSE-MIT
OutputDir=build
OutputBaseFilename=CineFS-Setup
SetupIconFile=AppIcon.ico
UninstallDisplayIcon={app}\{#MyAppExeName}
Compression=lzma2/ultra64
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed=x64compatible arm64
ArchitecturesInstallIn64BitMode=x64compatible arm64
ChangesAssociations=yes

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"
Name: "contextmenu"; Description: "Add 'Mount with CineFS' to Windows Explorer right-click menu (.MLV, .mcraw, .braw, and folders)"; GroupDescription: "Shell Integration:"
Name: "defenderexclusion"; Description: "Add CineFS virtual mount folder to Windows Defender exclusions (recommended for 4K RAW playback)"; GroupDescription: "Performance:"

[Files]
Source: "..\target\release\cinefs-win.exe"; DestDir: "{app}"; DestName: "CineFS.exe"; Flags: ignoreversion
Source: "..\target\release\cinefs-cli.exe"; DestDir: "{app}"; DestName: "cinefs-cli.exe"; Flags: ignoreversion
Source: "AppIcon.ico"; DestDir: "{app}"; Flags: ignoreversion
; Optional bundled WinFsp installer (downloaded by CI or build_windows.ps1 into windows\deps\winfsp.msi)
Source: "deps\winfsp.msi"; DestDir: "{tmp}"; Flags: ignoreversion deleteafterinstall skipifsourcedoesntexist

[Icons]
Name: "{group}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; IconFilename: "{app}\AppIcon.ico"
Name: "{autodesktop}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; IconFilename: "{app}\AppIcon.ico"; Tasks: desktopicon

[Registry]
; .mlv file association & right-click "Mount with CineFS"
Root: HKCR; Subkey: ".mlv\OpenWithProgids"; ValueType: string; ValueName: "CineFS.RAWClip"; ValueData: ""; Flags: uninsdeletevalue; Tasks: contextmenu
Root: HKCR; Subkey: ".mcraw\OpenWithProgids"; ValueType: string; ValueName: "CineFS.RAWClip"; ValueData: ""; Flags: uninsdeletevalue; Tasks: contextmenu
Root: HKCR; Subkey: ".braw\OpenWithProgids"; ValueType: string; ValueName: "CineFS.RAWClip"; ValueData: ""; Flags: uninsdeletevalue; Tasks: contextmenu
Root: HKCR; Subkey: "CineFS.RAWClip"; ValueType: string; ValueName: ""; ValueData: "RAW Video Clip (.MLV / .mcraw / .braw)"; Flags: uninsdeletekey; Tasks: contextmenu
Root: HKCR; Subkey: "CineFS.RAWClip\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: """{app}\AppIcon.ico"",0"; Tasks: contextmenu
Root: HKCR; Subkey: "CineFS.RAWClip\shell\open"; ValueType: string; ValueName: ""; ValueData: "Mount with CineFS"; Tasks: contextmenu
Root: HKCR; Subkey: "CineFS.RAWClip\shell\open\Icon"; ValueType: string; ValueName: ""; ValueData: """{app}\AppIcon.ico"""; Tasks: contextmenu
Root: HKCR; Subkey: "CineFS.RAWClip\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\{#MyAppExeName}"" ""%1"""; Tasks: contextmenu

; Folder right-click "Mount with CineFS"
Root: HKCR; Subkey: "Directory\shell\CineFS"; ValueType: string; ValueName: ""; ValueData: "Mount with CineFS"; Flags: uninsdeletekey; Tasks: contextmenu
Root: HKCR; Subkey: "Directory\shell\CineFS"; ValueType: string; ValueName: "Icon"; ValueData: """{app}\AppIcon.ico"""; Tasks: contextmenu
Root: HKCR; Subkey: "Directory\shell\CineFS\command"; ValueType: string; ValueName: ""; ValueData: """{app}\{#MyAppExeName}"" ""%V"""; Tasks: contextmenu

[Run]
; Silently install WinFsp if not already installed
Filename: "msiexec.exe"; Parameters: "/i ""{tmp}\winfsp.msi"" /qn /norestart"; StatusMsg: "Installing WinFsp virtual filesystem driver..."; Check: NeedsWinFspInstall; Flags: waituntilterminated
; Optional Windows Defender exclusion for %USERPROFILE%\CineFS_Mounts
Filename: "powershell.exe"; Parameters: "-NoProfile -NonInteractive -WindowStyle Hidden -Command ""Add-MpPreference -ExclusionPath '$env:USERPROFILE\CineFS_Mounts' -ErrorAction SilentlyContinue"""; StatusMsg: "Configuring Windows Defender exclusion for CineFS virtual mounts..."; Tasks: defenderexclusion; Flags: runhidden waituntilterminated
; Launch CineFS after install
Filename: "{app}\{#MyAppExeName}"; Description: "{cm:LaunchProgram,{#StringChange(MyAppName, '&', '&&')}}"; Flags: nowait postinstall skipifsilent

[Code]
function NeedsWinFspInstall(): Boolean;
var
  InstallDir: String;
begin
  Result := not FileExists(ExpandConstant('{tmp}\winfsp.msi'));
  if Result then
  begin
    Result := False;
    Exit;
  end;
  if RegQueryStringValue(HKLM32, 'SOFTWARE\WinFsp', 'InstallDir', InstallDir) or
     RegQueryStringValue(HKLM64, 'SOFTWARE\WinFsp', 'InstallDir', InstallDir) then
  begin
    Result := False;
  end
  else
  begin
    Result := True;
  end;
end;
