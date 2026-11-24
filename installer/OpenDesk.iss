; Instalador do OpenDesk (Inno Setup 6.7 ou mais novo).
;
; Gere com installer\build.ps1, que compila o app e passa a versão do
; Cargo.toml. Direto pelo compilador:
;
;   iscc /DAppVersion=0.1.0 installer\OpenDesk.iss
;
; O instalador coloca o OpenDesk em Arquivos de Programas, cria os atalhos,
; libera o programa no firewall e, se a pessoa quiser (vem marcado), instala o
; serviço do controle total. A pasta é fixa: o serviço só confia no
; executável que está lá (veja crates/opendesk-platform/src/windows/service.rs).

#ifndef AppVersion
  #error Informe a versão: iscc /DAppVersion=x.y.z OpenDesk.iss
#endif

#define AppName "OpenDesk"
#define AppExe "OpenDesk.exe"
#define Root ".."

[Setup]
AppId={{CC95DB62-31DA-4CA9-B9B6-90301CD7B3A3}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher=Vemryx
AppPublisherURL=https://vemryx.com
AppSupportURL=https://github.com/vemryx/OpenDesk
AppUpdatesURL=https://github.com/vemryx/OpenDesk/releases
AppCopyright=Código aberto, licença Apache-2.0
VersionInfoVersion={#AppVersion}
VersionInfoDescription=Instalador do OpenDesk
VersionInfoProductName={#AppName}

DefaultDirName={autopf}\{#AppName}
DisableDirPage=yes
DisableProgramGroupPage=yes
DisableWelcomePage=yes
DisableReadyPage=yes
UsePreviousTasks=yes

ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
PrivilegesRequired=admin
; Os valores do usuário (abrir com o Windows) são de quem instala.
UsedUserAreasWarning=no

OutputDir={#Root}\target\installer
OutputBaseFilename={#AppName}-Setup-{#AppVersion}
SetupIconFile={#Root}\apps\opendesk\assets\icon.ico
UninstallDisplayIcon={app}\{#AppExe}
UninstallDisplayName={#AppName}
WizardStyle=modern dynamic windows11
WizardImageFile=wizard.png
WizardSmallImageFile=wizard-small.png
WizardSizePercent=100
Compression=lzma2/ultra64
SolidCompression=yes
CloseApplications=no
RestartApplications=no

[Languages]
Name: "ptbr"; MessagesFile: "compiler:Languages\BrazilianPortuguese.isl"

[CustomMessages]
ptbr.TaskDesktop=Criar um atalho na área de trabalho
ptbr.TaskAutostart=Abrir o OpenDesk com o Windows (minimizado), para poder ser acessado a qualquer hora
ptbr.TaskFullControl=Ativar o controle total: quem acessa também alcança janelas de administrador, o aviso de permissão do Windows e a tela de bloqueio
ptbr.StatusFirewall=Liberando o OpenDesk no firewall...
ptbr.StatusService=Ativando o controle total...
ptbr.LaunchApp=Abrir o OpenDesk agora

[Messages]
ptbr.SelectTasksLabel2=O OpenDesk será instalado em Arquivos de Programas. Escolha o que mais o instalador deve fazer e clique em Instalar.
ptbr.FinishedLabel=O OpenDesk está instalado. Abra-o pelo atalho e passe o ID que aparece na tela para quem vai acessar este computador.

[Tasks]
Name: "desktopicon"; Description: "{cm:TaskDesktop}"
Name: "autostart"; Description: "{cm:TaskAutostart}"
Name: "fullcontrol"; Description: "{cm:TaskFullControl}"; Check: not ServiceWasInstalled

[Files]
Source: "{#Root}\target\release\opendesk.exe"; DestDir: "{app}"; DestName: "{#AppExe}"; Flags: ignoreversion
Source: "{#Root}\LICENSE"; DestDir: "{app}"; DestName: "LICENSE.txt"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\{#AppName}"; Filename: "{app}\{#AppExe}"; Comment: "Acesso remoto ponto a ponto"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExe}"; Comment: "Acesso remoto ponto a ponto"; Tasks: desktopicon

[Registry]
; O mesmo registro que a opção "Abrir com o Windows" do app grava e lê.
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "{#AppName}"; ValueData: """{app}\{#AppExe}"" --minimized"; Tasks: autostart

[Run]
; Como a pergunta do Windows faria na primeira execução: redes privadas e de domínio.
Filename: "{sys}\netsh.exe"; Parameters: "advfirewall firewall delete rule name=""{#AppName}"""; Flags: runhidden waituntilterminated
Filename: "{sys}\netsh.exe"; Parameters: "advfirewall firewall add rule name=""{#AppName}"" dir=in action=allow program=""{app}\{#AppExe}"" protocol=UDP profile=private,domain enable=yes"; StatusMsg: "{cm:StatusFirewall}"; Flags: runhidden waituntilterminated
; Instala o serviço, ou o atualiza para esta versão se já existia.
Filename: "{app}\{#AppExe}"; Parameters: "--install-service"; StatusMsg: "{cm:StatusService}"; Flags: runhidden waituntilterminated; Check: WantsService
; O app abre como quem está no computador, não como administrador.
Filename: "{app}\{#AppExe}"; Description: "{cm:LaunchApp}"; Flags: nowait postinstall skipifsilent runasoriginaluser

[UninstallRun]
; Com o instalador presente, isto remove só o serviço; o resto sai abaixo.
Filename: "{app}\{#AppExe}"; Parameters: "--uninstall-service"; Flags: runhidden waituntilterminated; RunOnceId: "RemoveService"
Filename: "{sys}\taskkill.exe"; Parameters: "/f /t /im {#AppExe}"; Flags: runhidden waituntilterminated; RunOnceId: "CloseApp"
Filename: "{sys}\netsh.exe"; Parameters: "advfirewall firewall delete rule name=""{#AppName}"""; Flags: runhidden waituntilterminated; RunOnceId: "RemoveFirewallRule"

[UninstallDelete]
Type: files; Name: "{app}\opendesk-service.log"
; A pasta pode ter sido criada pelo botão "Instalar" do app, antes do instalador.
Type: dirifempty; Name: "{app}"

[Code]
var
  ServiceExisted: Boolean;

function RunHidden(const Exe, Params: String): Integer;
begin
  if not Exec(Exe, Params, '', SW_HIDE, ewWaitUntilTerminated, Result) then
    Result := -1;
end;

function InitializeSetup(): Boolean;
begin
  // 0 quando o serviço existe; 1060 quando não.
  ServiceExisted := RunHidden(ExpandConstant('{sys}\sc.exe'), 'query {#AppName}') = 0;
  Result := True;
end;

function ServiceWasInstalled(): Boolean;
begin
  Result := ServiceExisted;
end;

// Quem já tinha o controle total continua com ele, agora nesta versão.
function WantsService(): Boolean;
begin
  Result := ServiceExisted or WizardIsTaskSelected('fullcontrol');
end;

// Solta o executável antigo: o serviço, o agente e o app aberto o seguram.
function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  if ServiceExisted then
    RunHidden(ExpandConstant('{sys}\net.exe'), 'stop {#AppName}');
  RunHidden(ExpandConstant('{sys}\taskkill.exe'), '/f /t /im {#AppExe}');
  Sleep(500);
  Result := '';
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  Command: String;
begin
  // A opção "Abrir com o Windows" pode ter sido ligada pelo app depois.
  if (CurUninstallStep = usPostUninstall)
    and RegQueryStringValue(HKCU, 'Software\Microsoft\Windows\CurrentVersion\Run', '{#AppName}', Command)
    and (Pos(Lowercase(ExpandConstant('{app}')), Lowercase(Command)) > 0) then
    RegDeleteValue(HKCU, 'Software\Microsoft\Windows\CurrentVersion\Run', '{#AppName}');
end;
