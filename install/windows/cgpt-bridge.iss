; cgpt-bridge - Windows setup (.exe) built with Inno Setup 6.
;
; One file for people who do not want PowerShell: it unpacks the release
; bundle into Setup's temp folder and runs the bundle's own install.ps1,
; which does all the work (files in %LOCALAPPDATA%\cgpt-bridge, native host,
; gateway config and token, OpenCode provider and agent, autostart, start and
; check). Setup itself keeps only its uninstaller, in
; %LOCALAPPDATA%\cgpt-bridge\setup, and an entry in "Installed apps"; that
; uninstaller runs uninstall.ps1. Per user, no admin rights.
;
; What it cannot do: load the extension into Chrome (Chrome installs
; extensions only from the Web Store or by hand). The last page says how, the
; extension's folder is put on the clipboard and Chrome can be opened at
; chrome://extensions.
;
; Built by scripts/build-setup.ps1 (CI: the Windows build job):
;   ISCC /DBundleDir=<unpacked bundle> /DAppVersion=<version> /DOutName=<name> /O<dir> cgpt-bridge.iss
; The setup's exit code is 1 when install.ps1 failed (its output is in
; %LOCALAPPDATA%\cgpt-bridge\setup\install.log).

#ifndef BundleDir
  #error Pass /DBundleDir=<unpacked release bundle>
#endif
#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef OutName
  #define OutName "cgpt-bridge-setup"
#endif

[Setup]
; Never change: Windows finds an installed copy (update, uninstall) by it.
AppId={{6F2A4C1E-8B3D-4E7A-9C51-2D0B7E4A9F13}
AppName=cgpt-bridge
AppVersion={#AppVersion}
AppVerName=cgpt-bridge {#AppVersion}
AppPublisher=cgpt-bridge
AppPublisherURL=https://github.com/elenik72/cgpt-bridge
DefaultDirName={localappdata}\cgpt-bridge\setup
DisableDirPage=yes
DisableProgramGroupPage=yes
DisableReadyPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0.17763
OutputBaseFilename={#OutName}
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
UninstallDisplayName=cgpt-bridge
UninstallDisplayIcon={sys}\shell32.dll,13
CloseApplications=no
RestartApplications=no
SetupLogging=yes

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"
Name: "russian"; MessagesFile: "compiler:Languages\Russian.isl"

[CustomMessages]
english.TaskAgent=Add the OpenCode agent cgpt and its subagent cgpt-sub (they ask before every command, file edit, web access, MCP tool and skill)
russian.TaskAgent=Добавить в OpenCode агента cgpt и его подагента cgpt-sub (спрашивают перед каждой командой, правкой файла, обращением в интернет, инструментом MCP и навыком)
english.Configuring=Setting up cgpt-bridge (Chrome native host, gateway, OpenCode)...
russian.Configuring=Настраиваю cgpt-bridge (связь с Chrome, шлюз, OpenCode)...
english.OpenChrome=Open the Chrome extensions page
russian.OpenChrome=Открыть страницу расширений Chrome
english.DoneOk=cgpt-bridge is installed and the gateway is running.%n%nOne step is left, in Chrome:%n1. On chrome://extensions turn on "Developer mode" (top right).%n2. Click "Load unpacked" and choose the folder%n    %1%n    (its path is on the clipboard: paste it into the folder dialog).%n3. Open chatgpt.com and log in.%n%nThen in OpenCode press Tab to switch to the agent cgpt.
russian.DoneOk=cgpt-bridge установлен, шлюз запущен.%n%nОстался один шаг, в Chrome:%n1. На странице chrome://extensions включите «Режим разработчика» (справа вверху).%n2. Нажмите «Загрузить распакованное расширение» и выберите папку%n    %1%n    (путь уже в буфере обмена: вставьте его в окно выбора папки).%n3. Откройте chatgpt.com и войдите в аккаунт.%n%nПотом в OpenCode нажмите Tab, чтобы выбрать агента cgpt.
english.DoneFailed=The setup of cgpt-bridge did not finish.%n%nWhat went wrong is in%n    %1%n%nFix it (for example, free port 8765) and run this setup again.
russian.DoneFailed=Настройка cgpt-bridge не завершилась.%n%nЧто пошло не так, записано в файле%n    %1%n%nИсправьте причину (например, освободите порт 8765) и запустите установку ещё раз.
english.Failed=The setup of cgpt-bridge did not finish. Details: %1
russian.Failed=Настройка cgpt-bridge не завершилась. Подробности: %1
english.RemoveExtension=cgpt-bridge is removed. Remove its extension in Chrome too: chrome://extensions, card cgpt-bridge, "Remove".
russian.RemoveExtension=cgpt-bridge удалён. Удалите и его расширение в Chrome: chrome://extensions, карточка cgpt-bridge, «Удалить».

[Tasks]
Name: "agent"; Description: "{cm:TaskAgent}"

[Files]
; The bundle as users get it; Setup deletes its temp folder at the end.
Source: "{#BundleDir}\*"; DestDir: "{tmp}\bundle"; Flags: recursesubdirs createallsubdirs ignoreversion

[Run]
Filename: "{code:ChromeExe}"; Parameters: "chrome://extensions"; Description: "{cm:OpenChrome}"; Flags: postinstall nowait skipifsilent; Check: CanOpenChrome

[UninstallRun]
; Removes what install.ps1 created (it keeps edited OpenCode files as .bak).
Filename: "powershell.exe"; Parameters: "-NoProfile -ExecutionPolicy Bypass -File ""{localappdata}\cgpt-bridge\uninstall.ps1"""; Flags: runhidden waituntilterminated; RunOnceId: "RemoveCgptBridge"

[UninstallDelete]
Type: files; Name: "{app}\install.log"
Type: dirifempty; Name: "{app}"
Type: dirifempty; Name: "{localappdata}\cgpt-bridge"

[Code]
var
  InstallOk: Boolean;

function ExtensionDir: String;
begin
  Result := ExpandConstant('{localappdata}\cgpt-bridge\extension');
end;

function InstallLog: String;
begin
  Result := ExpandConstant('{app}\install.log');
end;

// A PowerShell single-quoted string: no expansion, a quote is doubled.
function PsQuote(S: String): String;
begin
  StringChangeEx(S, '''', '''''', True);
  Result := '''' + S + '''';
end;

function ChromeExe(Param: String): String;
begin
  if not RegQueryStringValue(HKCU, 'Software\Microsoft\Windows\CurrentVersion\App Paths\chrome.exe', '', Result) then
    if not RegQueryStringValue(HKLM, 'Software\Microsoft\Windows\CurrentVersion\App Paths\chrome.exe', '', Result) then
      Result := '';
end;

function CanOpenChrome: Boolean;
begin
  Result := InstallOk and (ChromeExe('') <> '') and FileExists(ChromeExe(''));
end;

// Runs the bundle's install.ps1 with Windows PowerShell; everything it says
// goes to install.log next to the uninstaller.
procedure RunInstallScript;
var
  Params: String;
  Code: Integer;
begin
  Params := '-NoProfile -ExecutionPolicy Bypass -Command "& ' +
    PsQuote(ExpandConstant('{tmp}\bundle\install.ps1'));
  if not WizardIsTaskSelected('agent') then
    Params := Params + ' -NoAgent';
  Params := Params + ' *> ' + PsQuote(InstallLog) + '; exit $LASTEXITCODE"';
  Log('install.ps1: powershell.exe ' + Params);
  InstallOk := Exec('powershell.exe', Params, '', SW_HIDE, ewWaitUntilTerminated, Code) and (Code = 0);
  Log(Format('install.ps1 exit code: %d', [Code]));
end;

procedure CopyToClipboard(Text: String);
var
  Code: Integer;
begin
  Exec('powershell.exe', '-NoProfile -Command "Set-Clipboard -Value ' + PsQuote(Text) + '"',
    '', SW_HIDE, ewWaitUntilTerminated, Code);
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssPostInstall then
  begin
    WizardForm.StatusLabel.Caption := CustomMessage('Configuring');
    ForceDirectories(ExpandConstant('{app}'));
    RunInstallScript;
    if InstallOk then
    begin
      if not WizardSilent then
        CopyToClipboard(ExtensionDir);
    end
    else
      SuppressibleMsgBox(FmtMessage(CustomMessage('Failed'), [InstallLog]), mbError, MB_OK, IDOK);
  end;
end;

procedure CurPageChanged(CurPageID: Integer);
begin
  if CurPageID = wpFinished then
  begin
    if InstallOk then
      WizardForm.FinishedLabel.Caption := FmtMessage(CustomMessage('DoneOk'), [ExtensionDir])
    else
      WizardForm.FinishedLabel.Caption := FmtMessage(CustomMessage('DoneFailed'), [InstallLog]);
    // The longer text pushes the "Open Chrome" check box down.
    WizardForm.AdjustLabelHeight(WizardForm.FinishedLabel);
    WizardForm.RunList.Top := WizardForm.FinishedLabel.Top + WizardForm.FinishedLabel.Height + ScaleY(12);
  end;
end;

// Setup's exit code: 1 when install.ps1 failed (for silent installs).
function GetCustomSetupExitCode: Integer;
begin
  if InstallOk then
    Result := 0
  else
    Result := 1;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if (CurUninstallStep = usPostUninstall) and not UninstallSilent then
    MsgBox(CustomMessage('RemoveExtension'), mbInformation, MB_OK);
end;
