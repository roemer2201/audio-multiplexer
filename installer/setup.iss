; Inno Setup script for audio-multiplexer.
;
; Compiled by the release workflow on windows-latest, where Inno Setup 6
; is preinstalled (see .github/workflows/release.yml):
;   ISCC.exe /DAppVersion=<x.y.z> installer\setup.iss
; Expects the release binary at ..\target\release\audio-multiplexer.exe.
;
; The installer is per-user (no administrator rights, installs below
; %LOCALAPPDATA%\Programs), which also keeps unsigned-binary friction low.
; Note: the virtual audio driver prerequisite (see README) is intentionally
; NOT bundled or installed here; the app is driver-agnostic.

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif

[Setup]
AppId={{7E9B7C41-4D0C-4B9A-9A63-2F3D9E5A1C58}
AppName=Audio Multiplexer
AppVersion={#AppVersion}
AppPublisher=roemer2201
AppPublisherURL=https://github.com/roemer2201/audio-multiplexer
AppSupportURL=https://github.com/roemer2201/audio-multiplexer/issues
DefaultDirName={autopf}\Audio Multiplexer
DefaultGroupName=Audio Multiplexer
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
OutputDir=Output
OutputBaseFilename=audio-multiplexer-setup-{#AppVersion}
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
UninstallDisplayIcon={app}\audio-multiplexer.exe
LicenseFile=..\LICENSE

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"
Name: "german"; MessagesFile: "compiler:Languages\German.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; \
    GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "..\target\release\audio-multiplexer.exe"; DestDir: "{app}"; \
    Flags: ignoreversion
Source: "..\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\Audio Multiplexer"; Filename: "{app}\audio-multiplexer.exe"
Name: "{autodesktop}\Audio Multiplexer"; \
    Filename: "{app}\audio-multiplexer.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\audio-multiplexer.exe"; \
    Description: "{cm:LaunchProgram,Audio Multiplexer}"; \
    Flags: nowait postinstall skipifsilent

; The per-user config (%APPDATA%\audio-multiplexer\config.toml) is kept on
; uninstall on purpose; list it here if it should ever be removed:
; [UninstallDelete]
