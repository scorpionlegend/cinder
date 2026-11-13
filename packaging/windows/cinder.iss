; Inno Setup script for the Cinder setup .exe.
;
; ISCC is driven from .github/workflows/release.yml, which sets:
;   CINDER_STAGE_DIR            directory holding the unpacked bundle
;   CINDER_WIN_VERSION          display version, e.g. 0.1.0-pre.11
;   CINDER_WIN_VERSION_NUMERIC  four-part numeric version, e.g. 0.1.0.11
;   CINDER_BUNDLE_VERSION       version used in the output file name
;
; The file list is a recursive wildcard over the staged bundle, so the CEF
; runtime does not have to be enumerated here.

#define AppName "Cinder"
#define AppPublisher "Allen Schoepflin"
#define AppUrl "https://github.com/scorpionlegend/cinder"
#define StageDir GetEnv("CINDER_STAGE_DIR")
#define DisplayVersion GetEnv("CINDER_WIN_VERSION")
#define NumericVersion GetEnv("CINDER_WIN_VERSION_NUMERIC")
#define BundleVersion GetEnv("CINDER_BUNDLE_VERSION")

[Setup]
; A stable AppId keeps upgrades in place instead of installing a second copy.
AppId={{39CABCE0-D10B-4546-8E97-DF43DC5A65AD}
AppName={#AppName}
AppVersion={#DisplayVersion}
AppVerName={#AppName} {#DisplayVersion}
AppPublisher={#AppPublisher}
AppPublisherURL={#AppUrl}
AppSupportURL={#AppUrl}
AppUpdatesURL={#AppUrl}/releases
VersionInfoVersion={#NumericVersion}
VersionInfoProductVersion={#NumericVersion}
DefaultDirName={autopf}\Cinder
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
DisableDirPage=auto
UsePreviousAppDir=yes
OutputDir=dist
OutputBaseFilename=cinder-{#BundleVersion}-x86_64-pc-windows-msvc-setup
Compression=lzma2/max
SolidCompression=yes
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
PrivilegesRequired=admin
UninstallDisplayIcon={app}\cinder.exe
UninstallDisplayName={#AppName} {#DisplayVersion}
WizardStyle=modern
SetupLogging=yes

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "Create a desktop shortcut"; GroupDescription: "Additional shortcuts:"; Flags: unchecked

[Files]
Source: "{#StageDir}\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{group}\{#AppName}"; Filename: "{app}\cinder.exe"; WorkingDir: "{app}"
Name: "{group}\Uninstall {#AppName}"; Filename: "{uninstallexe}"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\cinder.exe"; WorkingDir: "{app}"; Tasks: desktopicon

[Run]
Filename: "{app}\cinder.exe"; Description: "Launch {#AppName}"; Flags: nowait postinstall skipifsilent
