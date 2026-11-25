; FastPin installer — build with Inno Setup 6's ISCC.exe (on PATH, or found under
; "C:\Program Files (x86)\Inno Setup 6" / "%LOCALAPPDATA%\Programs\Inno Setup 6"):
;   ISCC.exe installer\fastpin.iss
; Payload mirrors the portable layout: FastPin.exe + DirectML.dll + models\v6
; plus LICENSE / NOTICE / COPYRIGHT.snow-shot for GPL attribution, README.md for
; the source/build instructions, and LICENSE-APACHE for the vendored GPUI code.
; The models are exactly what ocr::models_dir expects next to the exe. The
; themes\ directory is loaded at runtime by gpui-kit's ThemeRegistry, so it
; ships next to the exe.

// release.yml injects /DAppVersion=<tag>; the fallback matches Cargo.toml.
#ifndef AppVersion
#define AppVersion "0.3.0"
#endif

[Setup]
AppId={{8F3A5C2E-9B7D-4E1F-A6C3-2D5E8F0B1A47}
AppName=FastPin
AppVersion={#AppVersion}
AppPublisher=yandongqiang
AppPublisherURL=https://github.com/yandongqiang/FastPin
AppSupportURL=https://github.com/yandongqiang/FastPin/issues
AppUpdatesURL=https://github.com/yandongqiang/FastPin/releases/latest
; Default to a per-user install (no UAC prompt). The updater asks which scope
; the user wants and passes /CURRENTUSER or /ALLUSERS. UsePreviousPrivileges=no
; makes a manually launched setup ask as well instead of silently reusing the
; old per-user mode. Both install scopes are supported.
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
UsePreviousPrivileges=no
DefaultDirName={autopf}\FastPin
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
; 开机启动由应用自身在设置页管理（HKCU，当前用户）。安装器在 /ALLUSERS
; 提权上下文里写 HKCU 会落到管理员账户，因此不再提供这个选项。
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "..\target\release\FastPin.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\target\release\DirectML.dll"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\NOTICE"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\COPYRIGHT.snow-shot"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\third_party\gpui\LICENSE-APACHE"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\models\v6\*"; DestDir: "{app}\models\v6"; Flags: ignoreversion recursesubdirs createallSubdirs
Source: "..\themes\*"; DestDir: "{app}\themes"; Flags: ignoreversion recursesubdirs createallSubdirs

[Icons]
Name: "{group}\FastPin"; Filename: "{app}\FastPin.exe"
Name: "{group}\卸载 FastPin"; Filename: "{uninstallexe}"
Name: "{autodesktop}\FastPin"; Filename: "{app}\FastPin.exe"; Tasks: desktopicon

; 没有 [Registry] 段：开机启动由应用自身写当前用户 HKCU，旧值由
; migrate_legacy_run_value() 迁移。提权的 /ALLUSERS 安装不能在这里写 HKCU。

[Run]
Filename: "{app}\FastPin.exe"; Description: "{cm:LaunchProgram,FastPin}"; Flags: nowait postinstall skipifsilent
