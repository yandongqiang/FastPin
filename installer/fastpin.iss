; FastPin installer — build with:
;   "C:\Users\admin\AppData\Local\Programs\Inno Setup 6\ISCC.exe" installer\fastpin.iss
; Payload mirrors the portable layout: FastPin.exe + DirectML.dll + models\v6,
; which is exactly what ocr::models_dir expects next to the exe.

// release.yml injects /DAppVersion=<tag>; the fallback matches Cargo.toml.
#ifndef AppVersion
#define AppVersion "0.3.0"
#endif

[Setup]
AppId={{8F3A5C2E-9B7D-4E1F-A6C3-2D5E8F0B1A47}
AppName=FastPin
AppVersion={#AppVersion}
AppPublisher=Zyl0812
AppPublisherURL=https://github.com/yandongqiang/FastPin
AppSupportURL=https://github.com/yandongqiang/FastPin/issues
AppUpdatesURL=https://github.com/yandongqiang/FastPin/releases/latest
; Per-user install: no UAC prompt, matches how a tray screenshot tool is used.
PrivilegesRequired=lowest
DefaultDirName={localappdata}\Programs\FastPin
DefaultGroupName=FastPin
OutputDir=..\dist
OutputBaseFilename=FastPin-v{#AppVersion}-x64-setup
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
Compression=lzma2/ultra64
SolidCompression=yes
WizardStyle=modern
UninstallDisplayIcon={app}\FastPin.exe
SetupIconFile=..\assets\logo\fastpin.ico
CloseApplications=yes
RestartApplications=no

[Languages]
Name: "chinesesimplified"; MessagesFile: "compiler:Languages\ChineseSimplified.isl"
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked
Name: "autostart"; Description: "开机自动启动"; GroupDescription: "其他选项:"; Flags: unchecked

[Files]
Source: "..\target\release\FastPin.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\target\release\DirectML.dll"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\models\v6\*"; DestDir: "{app}\models\v6"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{group}\FastPin"; Filename: "{app}\FastPin.exe"
Name: "{group}\卸载 FastPin"; Filename: "{uninstallexe}"
Name: "{autodesktop}\FastPin"; Filename: "{app}\FastPin.exe"; Tasks: desktopicon

[Registry]
; ValueName matches settings::system::RUN_VALUE. An in-place upgrade also gets
; the app's own first-start migration, which moves a legacy autostart value.
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "FastPin"; ValueData: """{app}\FastPin.exe"""; Flags: uninsdeletevalue; Tasks: autostart

[Run]
Filename: "{app}\FastPin.exe"; Description: "{cm:LaunchProgram,FastPin}"; Flags: nowait postinstall skipifsilent
