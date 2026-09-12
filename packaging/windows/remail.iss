; Inno Setup script for remail.
;
; Build it with a release binary already in target\release:
;
;     iscc packaging\windows\remail.iss
;
; The version can be overridden from the command line, which is what the
; release workflow does:
;
;     iscc /DAppVersion=0.2.0 packaging\windows\remail.iss

#ifndef AppVersion
  #define AppVersion "0.1.0"
#endif

#define AppName "remail"
#define AppPublisher "Andrew Henshaw"
#define AppURL "https://github.com/ahenshaw/remail"
#define AppExe "remail.exe"

[Setup]
; Never change this GUID: it is how Windows recognises an existing install
; and offers to upgrade it rather than installing a second copy.
AppId={{2B8A1501-EFC1-4E4E-880E-DAE170779036}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher={#AppPublisher}
AppPublisherURL={#AppURL}
AppSupportURL={#AppURL}/issues
AppUpdatesURL={#AppURL}/releases
DefaultDirName={autopf}\{#AppName}
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
LicenseFile=..\..\LICENSE
OutputDir=..\..\dist
OutputBaseFilename=remail-{#AppVersion}-setup
SetupIconFile=..\..\assets\remail.ico
UninstallDisplayIcon={app}\{#AppExe}
UninstallDisplayName={#AppName}
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern

; Offer the choice rather than demanding elevation: a mail client is a
; per-user tool, and installing to the profile needs no administrator.
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog

; The binary is 64-bit only.
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; \
    GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "..\..\target\release\{#AppExe}"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\LICENSE"; DestDir: "{app}"; DestName: "LICENSE.txt"; Flags: ignoreversion
Source: "..\..\README.md"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\{#AppName}"; Filename: "{app}\{#AppExe}"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExe}"; Tasks: desktopicon

[Run]
Filename: "{app}\{#AppExe}"; Description: "{cm:LaunchProgram,{#AppName}}"; \
    Flags: nowait postinstall skipifsilent

[UninstallDelete]
; Settings and cache live in the user's profile and are deliberately left
; behind on uninstall, the same as every other mail client: removing the
; program should not throw away account configuration.
Type: dirifempty; Name: "{app}"
