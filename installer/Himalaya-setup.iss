; Himalaya Code — Inno Setup installer script
; Produces: Himalaya-setup-x.y.z-windows-x64.exe
;
; Build requirements:
;   Inno Setup 6.x  https://jrsoftware.org/isinfo.php
;
; Usage (from CI or locally):
;   iscc installer\Himalaya-setup.iss /DMyAppVersion=0.1.0 /DBinDir=rust\target\release
;
; Or compile with defaults (version=dev, BinDir=rust\target\release):
;   iscc installer\Himalaya-setup.iss

#ifndef MyAppVersion
  #define MyAppVersion "dev"
#endif

#ifndef BinDir
  #define BinDir "..\rust\target\release"
#endif

#define MyAppName      "Himalaya Code"
#define MyAppPublisher "Himalaya Code Contributors"
#define MyAppURL       "https://github.com/your-org/Himalaya-code"
#define MyAppExeName   "Himalaya.exe"
#define MyAppID        "{A1B2C3D4-E5F6-7890-ABCD-EF1234567890}"

[Setup]
AppId={{#MyAppID}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppPublisher={#MyAppPublisher}
AppPublisherURL={#MyAppURL}
AppSupportURL={#MyAppURL}
AppUpdatesURL={#MyAppURL}
DefaultDirName={autopf}\HimalayaCode
DefaultGroupName={#MyAppName}
AllowNoIcons=yes
; Single-file output
OutputDir=dist
OutputBaseFilename=Himalaya-setup-{#MyAppVersion}-windows-x64
Compression=lzma2/ultra64
SolidCompression=yes
; Require 64-bit Windows
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
; No elevation required for per-user install
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
WizardStyle=modern
; Minimum Windows 10
MinVersion=10.0.17763
UninstallDisplayIcon={app}\{#MyAppExeName}
SetupIconFile=

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"
Name: "chinesesimplified"; MessagesFile: "compiler:Languages\ChineseSimplified.isl"

[Tasks]
Name: "addtopath"; Description: "Add Himalaya to the system PATH (recommended)"; GroupDescription: "Additional tasks:"; Flags: checked

[Files]
; Main binary — the only file needed (statically linked, no DLLs)
Source: "{#BinDir}\{#MyAppExeName}"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\Himalaya Code REPL"; Filename: "{app}\{#MyAppExeName}"
Name: "{group}\Uninstall Himalaya Code"; Filename: "{uninstallexe}"

[Registry]
; Add to PATH via registry (user-level)
Root: HKCU; Subkey: "Environment"; ValueType: expandsz; ValueName: "Path"; \
  ValueData: "{olddata};{app}"; \
  Check: NeedsAddPath(ExpandConstant('{app}')); \
  Tasks: addtopath

[Code]
// Helper: check whether {app} is already in the user PATH
function NeedsAddPath(Param: string): boolean;
var
  OrigPath: string;
begin
  if not RegQueryStringValue(HKCU, 'Environment', 'Path', OrigPath) then
  begin
    Result := True;
    exit;
  end;
  Result := Pos(';' + Uppercase(Param) + ';', ';' + Uppercase(OrigPath) + ';') = 0;
end;

[Run]
; Offer to open a terminal after install
Filename: "{cmd}"; Parameters: "/k echo Himalaya Code installed. Type 'Himalaya --version' to verify."; \
  Description: "Open a Command Prompt to verify installation"; \
  Flags: postinstall skipifsilent shellexec

[UninstallDelete]
Type: filesandordirs; Name: "{app}"
