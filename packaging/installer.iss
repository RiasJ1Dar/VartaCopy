; Build through tools/installer.py. No paths or hashes are taken from the network.
#ifndef PayloadDir
  #error PayloadDir must point to the verified staged payload
#endif
#ifndef AppVersion
  #error AppVersion is required
#endif
#ifndef ShellDirectory
  #error ShellDirectory is required
#endif
#ifndef ShellHash
  #error ShellHash is required
#endif
#ifndef BuildOutput
  #error BuildOutput is required
#endif

[Setup]
AppId={{2E086F3E-B877-40B8-9208-12E9E5679855}
AppName=VartaCopy
AppVersion={#AppVersion}
AppPublisher=VartaCopy contributors
AppPublisherURL=https://github.com/RiasJ1Dar/VartaCopy
AppSupportURL=https://github.com/RiasJ1Dar/VartaCopy/issues
AppUpdatesURL=https://github.com/RiasJ1Dar/VartaCopy/releases
DefaultDirName={autopf}\VartaCopy
DefaultGroupName=VartaCopy
DisableProgramGroupPage=yes
LicenseFile={#PayloadDir}\LICENSE.txt
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
PrivilegesRequired=admin
WizardStyle=modern dynamic
SetupMutex=Global\VartaCopy.Installer.v1
AppMutex=Local\VartaCopy.Running
CloseApplications=no
RestartApplications=no
AlwaysRestart=no
ChangesAssociations=yes
SetupLogging=yes
UninstallLogging=yes
Compression=lzma2
SolidCompression=yes
OutputDir={#BuildOutput}
OutputBaseFilename=VartaCopy-{#AppVersion}-windows-x64-setup
UninstallDisplayName=VartaCopy {#AppVersion}
UninstallDisplayIcon={app}\vartacopy-gui.exe

[Languages]
Name: "ukrainian"; MessagesFile: "compiler:Languages\Ukrainian.isl"
Name: "english"; MessagesFile: "compiler:Default.isl"

[CustomMessages]
ukrainian.TransferTask=Показувати вибір Explorer / VartaCopy при Ctrl+V та перетягуванні
ukrainian.DesktopTask=Створити ярлик на робочому столі
ukrainian.LaunchApp=Запустити VartaCopy
ukrainian.ReloadExplorer=Для завантаження нової інтеграції перезапустіть Провідник або вийдіть зі свого облікового запису та ввійдіть знову. Програма не перезапускає Провідник автоматично.
ukrainian.IntegrationFailed=Програму встановлено, але налаштування інтеграції не підтверджене. Попередні власні записи відновлюються журналом. Перевірте стан у налаштуваннях VartaCopy; подробиці є в журналі встановлення.
ukrainian.UninstallBlocked=Інтеграція змінена або її видалення не підтверджене. Файли програми залишені. Відновіть встановлення й повторіть видалення; подробиці є в журналі.
ukrainian.BadModule=Виявлено змінений файл компонента Провідника. Встановлення зупинено до зміни інтеграції.
english.TransferTask=Show Explorer / VartaCopy choice for paste and drag-and-drop
english.DesktopTask=Create a desktop shortcut
english.LaunchApp=Launch VartaCopy
english.ReloadExplorer=Restart File Explorer or sign out and in to load the updated integration. Setup does not restart Explorer automatically.
english.IntegrationFailed=The application was installed, but integration was not confirmed. The operation journal restores previous owned entries. Check the integration state in VartaCopy settings; see the setup log for details.
english.UninstallBlocked=Integration was changed or its removal was not confirmed. Application files were retained. Repair the installation and retry uninstalling; see the log for details.
english.BadModule=A changed Explorer component was found. Installation stopped before integration changes.

[Tasks]
Name: "explorertransfer"; Description: "{cm:TransferTask}"
Name: "desktopicon"; Description: "{cm:DesktopTask}"; Flags: unchecked

[Files]
Source: "{#PayloadDir}\vartacopy.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#PayloadDir}\vartacopy-gui.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#PayloadDir}\README.txt"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#PayloadDir}\EXPLORER.txt"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#PayloadDir}\LICENSE.txt"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#PayloadDir}\THIRD-PARTY.json"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#PayloadDir}\licenses\*"; DestDir: "{app}\licenses"; Flags: ignoreversion recursesubdirs createallsubdirs
; The content-addressed module is immutable, so a repair never overwrites a loaded DLL.
Source: "{#PayloadDir}\{#ShellDirectory}\vartacopy_transfer.dll"; DestDir: "{app}\{#ShellDirectory}"; Flags: onlyifdoesntexist uninsrestartdelete
Source: "{#PayloadDir}\integration-install.json"; DestDir: "{app}"; Flags: ignoreversion
; Stage helpers separately for read-only preflight before destination files change.
Source: "{#PayloadDir}\vartacopy.exe"; DestName: "vartacopy-check.exe"; Flags: dontcopy
Source: "{#PayloadDir}\vartacopy-gui.exe"; DestName: "vartacopy-gui.exe"; Flags: dontcopy
Source: "{#PayloadDir}\{#ShellDirectory}\vartacopy_transfer.dll"; DestName: "vartacopy_transfer.dll"; Flags: dontcopy

[Icons]
Name: "{group}\VartaCopy"; Filename: "{app}\vartacopy-gui.exe"
Name: "{autodesktop}\VartaCopy"; Filename: "{app}\vartacopy-gui.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\vartacopy-gui.exe"; Description: "{cm:LaunchApp}"; Flags: nowait postinstall skipifsilent runasoriginaluser

[Code]
var
  LastToolOutput: String;
  IntegrationFailed: Boolean;

procedure CaptureToolLine(const S: String; const Error, FirstLine: Boolean);
begin
  Log(S);
  LastToolOutput := LastToolOutput + S + #10;
end;

function Tool(const Exe, Action, Gui, Dll: String): Boolean;
var
  Code: Integer;
begin
  LastToolOutput := '';
  Code := -1;
  Result := ExecAndLogOutput(Exe,
    'integration ' + Action + ' --gui "' + Gui + '" --dll "' + Dll + '" --json',
    '', SW_SHOWNORMAL, ewWaitUntilTerminated, Code, @CaptureToolLine);
  if Result then Result := Code = 0;
  if not Result then Log(Format('VartaCopy integration action %s failed, code %d', [Action, Code]));
end;

function AppTool(const Action: String): Boolean;
begin
  Result := Tool(ExpandConstant('{app}\vartacopy.exe'), Action,
    ExpandConstant('{app}\vartacopy-gui.exe'),
    ExpandConstant('{app}\{#ShellDirectory}\vartacopy_transfer.dll'));
end;

function RemoveRegisteredModuleBelowApp: Boolean;
var
  Dll, Root: String;
begin
  Result := True;
  if RegQueryStringValue(HKLM64,
      'Software\Classes\CLSID\{91DD2708-74C6-4498-87B6-09420A94F66D}\InprocServer32', '', Dll) then begin
    Root := AddBackslash(ExpandFileName(ExpandConstant('{app}')));
    // A previous version below this installation must not retain a deleted GUI.
    // The native helper still verifies every owned key and the exact config.
    if CompareText(Copy(ExpandFileName(Dll), 1, Length(Root)), Root) = 0 then
      Result := Tool(ExpandConstant('{app}\vartacopy.exe'), 'remove',
        ExpandConstant('{app}\vartacopy-gui.exe'), Dll);
  end;
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  Module: String;
begin
  Result := '';
  Module := ExpandConstant('{app}\{#ShellDirectory}\vartacopy_transfer.dll');
  if FileExists(Module) and (CompareText(GetSHA256OfFile(Module), '{#ShellHash}') <> 0) then begin
    Result := CustomMessage('BadModule');
    Exit;
  end;
  if WizardIsTaskSelected('explorertransfer') then begin
    ExtractTemporaryFile('vartacopy-check.exe');
    ExtractTemporaryFile('vartacopy-gui.exe');
    ExtractTemporaryFile('vartacopy_transfer.dll');
    if not Tool(ExpandConstant('{tmp}\vartacopy-check.exe'), 'check',
        ExpandConstant('{tmp}\vartacopy-gui.exe'), ExpandConstant('{tmp}\vartacopy_transfer.dll')) then
      if not FileExists(ExpandConstant('{app}\explorer-integration-update.json')) then
        Result := CustomMessage('IntegrationFailed');
  end;
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssPostInstall then begin
    if WizardIsTaskSelected('explorertransfer') then begin
      IntegrationFailed := not AppTool('install');
    end else IntegrationFailed := not RemoveRegisteredModuleBelowApp;
    if IntegrationFailed then
      SuppressibleMsgBox(CustomMessage('IntegrationFailed'), mbError, MB_OK, IDOK);
  end;
end;

procedure CurPageChanged(CurPageID: Integer);
begin
  if CurPageID = wpFinished then begin
    if IntegrationFailed then WizardForm.FinishedLabel.Caption := CustomMessage('IntegrationFailed')
    else if WizardIsTaskSelected('explorertransfer') then
      WizardForm.FinishedLabel.Caption := WizardForm.FinishedLabel.Caption + #13#10#13#10 + CustomMessage('ReloadExplorer');
  end;
end;

function GetCustomSetupExitCode: Integer;
begin
  if IntegrationFailed then Result := 1 else Result := 0;
end;

function InitializeUninstall: Boolean;
begin
  Result := not CheckForMutexes('Global\VartaCopy.Installer.v1');
  if Result then CreateMutex('Global\VartaCopy.Installer.v1')
  else SuppressibleMsgBox(CustomMessage('UninstallBlocked'), mbError, MB_OK, IDOK);
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usUninstall then begin
    if not AppTool('status') then begin
      SuppressibleMsgBox(CustomMessage('UninstallBlocked'), mbError, MB_OK, IDOK);
      Abort;
    end;
    // Another installation's registrations must survive uninstalling this one.
    if (Pos('"state":"enabled"', LastToolOutput) > 0) or
       (Pos('"state":"pending_recovery"', LastToolOutput) > 0) then
      if not AppTool('remove') then begin
        SuppressibleMsgBox(CustomMessage('UninstallBlocked'), mbError, MB_OK, IDOK);
        Abort;
      end;
    if Pos('"state":"incomplete"', LastToolOutput) > 0 then begin
      SuppressibleMsgBox(CustomMessage('UninstallBlocked'), mbError, MB_OK, IDOK);
      Abort;
    end;
    if not RemoveRegisteredModuleBelowApp then begin
      SuppressibleMsgBox(CustomMessage('UninstallBlocked'), mbError, MB_OK, IDOK);
      Abort;
    end;
  end;
end;
