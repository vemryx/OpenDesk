//! O serviço do OpenDesk: instalar, remover, consultar e executar.
//!
//! O serviço roda como SYSTEM e faz uma coisa só: manter um [agente] na sessão
//! de quem está usando o computador. Ele não abre porta, não fala com a rede e
//! não guarda nada; toda a lógica continua no aplicativo.
//!
//! Instalar copia este executável para `Arquivos de Programas\OpenDesk`, uma
//! pasta em que só administradores escrevem. É isso que permite ao agente
//! confiar no executável que conecta a ele, e é por isso que o controle total
//! só vale para o OpenDesk aberto a partir de lá.
//!
//! [agente]: super::agent

use super::agent::{own_image, session_user_sid};
use super::{Owned, wide};
use anyhow::{Context, Result, anyhow, bail, ensure};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{
    ERROR_CALL_NOT_IMPLEMENTED, ERROR_CANCELLED, HANDLE, NO_ERROR, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::Security::{
    DuplicateTokenEx, SecurityIdentification, SetTokenInformation, TOKEN_ALL_ACCESS, TokenPrimary, TokenSessionId,
};
use windows::Win32::Storage::FileSystem::{MOVEFILE_DELAY_UNTIL_REBOOT, MoveFileExW};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree, IPersistFile,
};
use windows::Win32::System::RemoteDesktop::WTSGetActiveConsoleSessionId;
use windows::Win32::System::Services::{
    ChangeServiceConfig2W, ChangeServiceConfigW, CloseServiceHandle, ControlService, CreateServiceW, DeleteService,
    OpenSCManagerW, OpenServiceW, QUERY_SERVICE_CONFIGW, QueryServiceConfigW, QueryServiceStatus,
    RegisterServiceCtrlHandlerExW, SC_HANDLE, SC_MANAGER_ALL_ACCESS, SC_MANAGER_CONNECT, SERVICE_ACCEPT_SHUTDOWN,
    SERVICE_ACCEPT_STOP, SERVICE_ALL_ACCESS, SERVICE_AUTO_START, SERVICE_CONFIG_DESCRIPTION,
    SERVICE_CONTROL_INTERROGATE, SERVICE_CONTROL_SHUTDOWN, SERVICE_CONTROL_STOP, SERVICE_DESCRIPTIONW,
    SERVICE_ERROR_NORMAL, SERVICE_QUERY_CONFIG, SERVICE_QUERY_STATUS, SERVICE_RUNNING, SERVICE_STATUS,
    SERVICE_STATUS_CURRENT_STATE, SERVICE_STATUS_HANDLE, SERVICE_STOPPED, SERVICE_TABLE_ENTRYW,
    SERVICE_WIN32_OWN_PROCESS, SetServiceStatus, StartServiceCtrlDispatcherW, StartServiceW,
};
use windows::Win32::System::Threading::{
    CREATE_NO_WINDOW, CreateEventW, CreateProcessAsUserW, GetCurrentProcess, GetCurrentProcessId, GetExitCodeProcess,
    INFINITE, OpenProcessToken, PROCESS_INFORMATION, STARTUPINFOW, SetEvent, TerminateProcess, WaitForSingleObject,
};
use windows::Win32::UI::Shell::{
    FOLDERID_CommonPrograms, FOLDERID_ProgramFiles, FOLDERID_System, IShellLinkW, KF_FLAG_DEFAULT, SEE_MASK_NOASYNC,
    SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, SHGetKnownFolderPath, ShellExecuteExW, ShellLink,
};
use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;
use windows::core::{GUID, Interface, PCWSTR, PWSTR};

const NAME: &str = "OpenDesk";
const DESCRIPTION: &str = "Permite ao OpenDesk controlar janelas de administrador, o aviso de permissão do Windows e a tela de bloqueio durante um acesso remoto autorizado.";
const EXE: &str = "OpenDesk.exe";
const LOG: &str = "opendesk-service.log";

/// Como o serviço está neste computador.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServiceState {
    pub installed: bool,
    pub running: bool,
    /// O executável que o serviço usa.
    pub path: Option<String>,
    /// Este processo é esse executável. Só ele pode usar o controle total.
    pub current: bool,
}

struct Sc(SC_HANDLE);

impl Drop for Sc {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseServiceHandle(self.0);
        }
    }
}

fn manager(access: u32) -> windows::core::Result<Sc> {
    unsafe { OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), access) }.map(Sc)
}

fn open(manager: &Sc, access: u32) -> windows::core::Result<Sc> {
    unsafe { OpenServiceW(manager.0, PCWSTR(wide(NAME).as_ptr()), access) }.map(Sc)
}

fn status(service: &Sc) -> Option<SERVICE_STATUS_CURRENT_STATE> {
    let mut status = SERVICE_STATUS::default();
    unsafe { QueryServiceStatus(service.0, &mut status) }.ok()?;
    Some(status.dwCurrentState)
}

/// Consulta o serviço. Não exige administrador.
pub fn state() -> ServiceState {
    let Ok(manager) = manager(SC_MANAGER_CONNECT) else { return ServiceState::default() };
    let Ok(service) = open(&manager, SERVICE_QUERY_STATUS | SERVICE_QUERY_CONFIG) else {
        return ServiceState::default();
    };
    let path = binary_path(&service);
    let current = match (&path, own_image()) {
        (Some(path), Ok(own)) => path.eq_ignore_ascii_case(&own),
        _ => false,
    };
    ServiceState { installed: true, running: status(&service) == Some(SERVICE_RUNNING), path, current }
}

/// O executável registrado no serviço, sem as aspas e sem os argumentos.
fn binary_path(service: &Sc) -> Option<String> {
    let mut buffer = [0u64; 1024];
    let mut needed = 0;
    unsafe {
        QueryServiceConfigW(service.0, Some(buffer.as_mut_ptr().cast()), size_of_val(&buffer) as u32, &mut needed)
    }
    .ok()?;
    let config = unsafe { &*buffer.as_ptr().cast::<QUERY_SERVICE_CONFIGW>() };
    let command = unsafe { config.lpBinaryPathName.to_string() }.ok()?;
    let path = match command.strip_prefix('"') {
        Some(rest) => rest.split('"').next()?,
        None => command.split(" --").next()?,
    };
    Some(path.to_string())
}

fn known_folder(id: &GUID) -> Result<PathBuf> {
    let path = unsafe { SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None) }.context("localizar pasta do Windows")?;
    let text = unsafe { path.to_string() };
    unsafe { CoTaskMemFree(Some(path.0.cast())) };
    Ok(PathBuf::from(text.context("pasta do Windows com nome inválido")?))
}

/// Onde o OpenDesk instalado fica. Vem do Windows, não de variável de
/// ambiente, que quem pediu a instalação poderia ter trocado.
fn install_dir() -> Result<PathBuf> {
    Ok(known_folder(&FOLDERID_ProgramFiles)?.join("OpenDesk"))
}

fn shortcut_path() -> Result<PathBuf> {
    Ok(known_folder(&FOLDERID_CommonPrograms)?.join("OpenDesk.lnk"))
}

/// Onde o serviço e o agente registram o que fazem: ao lado do executável
/// instalado, numa pasta em que só administradores escrevem. `None` quando
/// este processo não é o instalado.
pub fn log_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = install_dir().ok()?;
    (exe.parent()? == dir).then(|| dir.join(LOG))
}

fn stop_and_wait(service: &Sc) {
    let mut ignored = SERVICE_STATUS::default();
    unsafe {
        let _ = ControlService(service.0, SERVICE_CONTROL_STOP, &mut ignored);
    }
    let deadline = Instant::now() + Duration::from_secs(15);
    while status(service).is_some_and(|state| state != SERVICE_STOPPED) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Instala (ou atualiza) e inicia o serviço. Precisa de administrador; o
/// aplicativo chega aqui por [`elevate`].
pub fn install() -> Result<()> {
    let manager = manager(SC_MANAGER_ALL_ACCESS).context("instalar o serviço exige permissão de administrador")?;
    let existing = open(&manager, SERVICE_ALL_ACCESS).ok();
    if let Some(service) = &existing {
        // Solta o executável instalado para poder trocá-lo.
        stop_and_wait(service);
    }

    let dir = install_dir()?;
    let target = dir.join(EXE);
    let source = std::env::current_exe().context("localizar o executável do OpenDesk")?;
    if source != target {
        std::fs::create_dir_all(&dir).with_context(|| format!("criar {}", dir.display()))?;
        copy_when_free(&source, &target)?;
    }

    let command = wide(&format!("\"{}\" --service", target.display()));
    let name = wide(NAME);
    let service = match existing {
        Some(service) => {
            unsafe {
                ChangeServiceConfigW(
                    service.0,
                    SERVICE_WIN32_OWN_PROCESS,
                    SERVICE_AUTO_START,
                    SERVICE_ERROR_NORMAL,
                    PCWSTR(command.as_ptr()),
                    PCWSTR::null(),
                    None,
                    PCWSTR::null(),
                    PCWSTR::null(),
                    PCWSTR::null(),
                    PCWSTR(name.as_ptr()),
                )
            }
            .context("atualizar o serviço")?;
            service
        }
        None => Sc(unsafe {
            CreateServiceW(
                manager.0,
                PCWSTR(name.as_ptr()),
                PCWSTR(name.as_ptr()),
                SERVICE_ALL_ACCESS,
                SERVICE_WIN32_OWN_PROCESS,
                SERVICE_AUTO_START,
                SERVICE_ERROR_NORMAL,
                PCWSTR(command.as_ptr()),
                PCWSTR::null(),
                None,
                PCWSTR::null(),
                // Sem conta informada, o serviço roda como LocalSystem.
                PCWSTR::null(),
                PCWSTR::null(),
            )
        }
        .context("registrar o serviço")?),
    };

    let mut description = wide(DESCRIPTION);
    let info = SERVICE_DESCRIPTIONW { lpDescription: PWSTR(description.as_mut_ptr()) };
    unsafe {
        let _ = ChangeServiceConfig2W(service.0, SERVICE_CONFIG_DESCRIPTION, Some((&raw const info).cast()));
    }
    unsafe { StartServiceW(service.0, None) }.context("iniciar o serviço")?;

    if let Err(err) = shortcut_path().and_then(|link| create_shortcut(&target, &link)) {
        tracing::warn!("atalho no menu Iniciar não criado: {err:#}");
    }
    // Sem a regra, o Windows perguntaria de novo ao abrir o OpenDesk instalado,
    // que para o firewall é outro programa.
    if let Err(err) = firewall_rule(Some(&target)) {
        tracing::warn!("regra do firewall não criada: {err:#}");
    }
    Ok(())
}

/// Cria (com `Some`) ou remove a regra que deixa o OpenDesk instalado receber
/// conexões em redes privadas e de domínio, como a pergunta do Windows faria.
fn firewall_rule(program: Option<&Path>) -> Result<()> {
    use std::os::windows::process::CommandExt;
    // O caminho vem do Windows: num processo elevado, não se procura o `netsh` no PATH.
    let netsh = known_folder(&FOLDERID_System)?.join("netsh.exe");
    let run = |arguments: String| {
        std::process::Command::new(&netsh)
            .raw_arg(arguments)
            .creation_flags(CREATE_NO_WINDOW.0)
            .status()
            .context("executar o netsh")
    };
    // Remove antes de criar: instalar duas vezes não empilha regras.
    let removed = run(format!("advfirewall firewall delete rule name=\"{NAME}\""))?;
    let Some(program) = program else {
        return Ok(());
    };
    let _ = removed;
    let added = run(format!(
        "advfirewall firewall add rule name=\"{NAME}\" dir=in action=allow program=\"{}\" protocol=UDP profile=private,domain enable=yes",
        program.display()
    ))?;
    ensure!(added.success(), "o netsh recusou a regra ({added})");
    Ok(())
}

/// O executável instalado pode estar em uso por um agente que ainda está
/// terminando; insiste por alguns segundos antes de desistir.
fn copy_when_free(source: &Path, target: &Path) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match std::fs::copy(source, target) {
            Ok(_) => return Ok(()),
            Err(err) if Instant::now() < deadline => {
                tracing::debug!("{} ainda em uso: {err}", target.display());
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(err) => {
                return Err(anyhow!(err).context(format!(
                    "copiar o OpenDesk para {} (feche o OpenDesk instalado e tente de novo)",
                    target.display()
                )));
            }
        }
    }
}

/// Para e remove o serviço e o que a instalação criou. Precisa de administrador.
pub fn uninstall() -> Result<()> {
    let manager = manager(SC_MANAGER_ALL_ACCESS).context("remover o serviço exige permissão de administrador")?;
    if let Ok(service) = open(&manager, SERVICE_ALL_ACCESS) {
        stop_and_wait(&service);
        unsafe { DeleteService(service.0) }.context("remover o serviço")?;
    }
    if let Ok(link) = shortcut_path() {
        let _ = std::fs::remove_file(link);
    }
    let _ = firewall_rule(None);
    let dir = install_dir()?;
    let _ = std::fs::remove_file(dir.join(LOG));
    let exe = dir.join(EXE);
    if exe.exists() && std::fs::remove_file(&exe).is_err() {
        // Em uso (é o OpenDesk que pediu a remoção): o Windows apaga ao reiniciar.
        unsafe {
            let _ =
                MoveFileExW(PCWSTR(wide(&exe.to_string_lossy()).as_ptr()), PCWSTR::null(), MOVEFILE_DELAY_UNTIL_REBOOT);
            let _ =
                MoveFileExW(PCWSTR(wide(&dir.to_string_lossy()).as_ptr()), PCWSTR::null(), MOVEFILE_DELAY_UNTIL_REBOOT);
        }
    } else {
        let _ = std::fs::remove_dir(&dir);
    }
    Ok(())
}

fn create_shortcut(target: &Path, link: &Path) -> Result<()> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let shortcut: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).context("criar atalho")?;
        shortcut.SetPath(PCWSTR(wide(&target.to_string_lossy()).as_ptr())).context("definir o destino do atalho")?;
        shortcut.SetDescription(PCWSTR(wide("Acesso remoto ponto a ponto").as_ptr())).ok();
        let file: IPersistFile = shortcut.cast().context("gravar atalho")?;
        file.Save(PCWSTR(wide(&link.to_string_lossy()).as_ptr()), true).context("gravar atalho")?;
    }
    Ok(())
}

/// Executa este mesmo programa como administrador, com `argument`, e espera
/// terminar. O Windows pede a confirmação de quem está no computador. Bloqueia:
/// chame fora da thread da interface.
pub fn elevate(argument: &str) -> Result<()> {
    let exe = wide(&std::env::current_exe().context("localizar o executável do OpenDesk")?.to_string_lossy());
    let (verb, argument) = (wide("runas"), wide(argument));
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC,
        lpVerb: PCWSTR(verb.as_ptr()),
        lpFile: PCWSTR(exe.as_ptr()),
        lpParameters: PCWSTR(argument.as_ptr()),
        nShow: SW_HIDE.0,
        ..Default::default()
    };
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
    if let Err(err) = unsafe { ShellExecuteExW(&mut info) } {
        if err.code() == ERROR_CANCELLED.to_hresult() {
            bail!("A permissão de administrador não foi concedida.");
        }
        return Err(anyhow!(err).context("pedir permissão de administrador"));
    }
    ensure!(!info.hProcess.is_invalid(), "o Windows não iniciou a instalação");
    let process = Owned(info.hProcess);
    let mut code = 1;
    unsafe {
        WaitForSingleObject(process.0, INFINITE);
        GetExitCodeProcess(process.0, &mut code).context("ler o resultado da instalação")?;
    }
    ensure!(code == 0, "A operação falhou (código {code}).");
    Ok(())
}

// ─────────────────────────── Execução ───────────────────────────

/// O evento que manda o serviço parar, como inteiro para caber num `static`.
static STOP: OnceLock<isize> = OnceLock::new();

/// Ponto de entrada do processo `--service`. Só funciona quando é o Windows
/// que inicia o programa como serviço.
pub fn run() -> Result<()> {
    let mut name = wide(NAME);
    let table = [
        SERVICE_TABLE_ENTRYW { lpServiceName: PWSTR(name.as_mut_ptr()), lpServiceProc: Some(service_main) },
        SERVICE_TABLE_ENTRYW::default(),
    ];
    unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) }
        .context("este modo só funciona quando o Windows inicia o OpenDesk como serviço")
}

unsafe extern "system" fn service_main(_argc: u32, _argv: *mut PWSTR) {
    let Ok(stop) = (unsafe { CreateEventW(None, true, false, PCWSTR::null()) }) else { return };
    let _ = STOP.set(stop.0 as isize);
    let Ok(status) = (unsafe { RegisterServiceCtrlHandlerExW(PCWSTR(wide(NAME).as_ptr()), Some(control), None) })
    else {
        return;
    };
    report(status, SERVICE_RUNNING);
    tracing::info!("serviço iniciado");
    supervise(stop);
    tracing::info!("serviço parado");
    report(status, SERVICE_STOPPED);
}

unsafe extern "system" fn control(
    control: u32,
    _event: u32,
    _data: *mut core::ffi::c_void,
    _context: *mut core::ffi::c_void,
) -> u32 {
    match control {
        SERVICE_CONTROL_STOP | SERVICE_CONTROL_SHUTDOWN => {
            if let Some(&stop) = STOP.get() {
                unsafe {
                    let _ = SetEvent(HANDLE(stop as *mut _));
                }
            }
            NO_ERROR.0
        }
        SERVICE_CONTROL_INTERROGATE => NO_ERROR.0,
        _ => ERROR_CALL_NOT_IMPLEMENTED.0,
    }
}

fn report(handle: SERVICE_STATUS_HANDLE, state: SERVICE_STATUS_CURRENT_STATE) {
    let status = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        dwControlsAccepted: if state == SERVICE_RUNNING { SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN } else { 0 },
        ..Default::default()
    };
    unsafe {
        let _ = SetServiceStatus(handle, &status);
    }
}

/// A sessão que está no console (teclado e monitor do computador) e o usuário
/// nela. `None` enquanto ninguém entrou: sem usuário não há aplicativo para o
/// agente servir.
fn console_user() -> Option<(u32, String)> {
    let session = unsafe { WTSGetActiveConsoleSessionId() };
    if session == u32::MAX {
        return None;
    }
    Some((session, session_user_sid(session).ok()?))
}

/// Mantém um agente na sessão do console, trocando-o quando muda o usuário ou
/// a sessão, até o serviço ser parado.
fn supervise(stop: HANDLE) {
    let mut agent: Option<(Owned, (u32, String))> = None;
    let mut last_spawn: Option<Instant> = None;
    loop {
        let wanted = console_user();
        if let Some((process, key)) = &agent {
            let alive = unsafe { WaitForSingleObject(process.0, 0) } == WAIT_TIMEOUT;
            if !alive || Some(key) != wanted.as_ref() {
                kill(agent.take());
            }
        }
        // Um agente que cai não é recriado em rajada.
        let may_spawn = last_spawn.is_none_or(|at| at.elapsed() >= Duration::from_secs(2));
        if agent.is_none()
            && may_spawn
            && let Some(key) = wanted
        {
            last_spawn = Some(Instant::now());
            match spawn_agent(key.0) {
                Ok(process) => {
                    tracing::info!("agente iniciado na sessão {}", key.0);
                    agent = Some((process, key));
                }
                Err(err) => tracing::warn!("não foi possível iniciar o agente na sessão {}: {err:#}", key.0),
            }
        }
        if unsafe { WaitForSingleObject(stop, 1000) } == WAIT_OBJECT_0 {
            break;
        }
    }
    kill(agent);
}

fn kill(agent: Option<(Owned, (u32, String))>) {
    if let Some((process, _)) = agent {
        unsafe {
            let _ = TerminateProcess(process.0, 0);
            WaitForSingleObject(process.0, 3000);
        }
    }
}

/// Inicia este executável como agente, com o token SYSTEM do serviço, dentro
/// da sessão `session`.
fn spawn_agent(session: u32) -> Result<Owned> {
    let exe = std::env::current_exe().context("localizar o executável do OpenDesk")?;
    unsafe {
        let mut own = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_ALL_ACCESS, &mut own).context("abrir o token do serviço")?;
        let own = Owned(own);
        let mut token = HANDLE::default();
        DuplicateTokenEx(own.0, TOKEN_ALL_ACCESS, None, SecurityIdentification, TokenPrimary, &mut token)
            .context("duplicar o token do serviço")?;
        let token = Owned(token);
        SetTokenInformation(token.0, TokenSessionId, (&raw const session).cast(), size_of::<u32>() as u32)
            .context("apontar o token para a sessão do usuário")?;

        let application = wide(&exe.to_string_lossy());
        let mut command = wide(&format!("\"{}\" --agent --parent {}", exe.display(), GetCurrentProcessId()));
        let mut desktop = wide(r"winsta0\default");
        let startup = STARTUPINFOW {
            cb: size_of::<STARTUPINFOW>() as u32,
            lpDesktop: PWSTR(desktop.as_mut_ptr()),
            ..Default::default()
        };
        let mut created = PROCESS_INFORMATION::default();
        CreateProcessAsUserW(
            Some(token.0),
            PCWSTR(application.as_ptr()),
            Some(PWSTR(command.as_mut_ptr())),
            None,
            None,
            false,
            CREATE_NO_WINDOW,
            None,
            PCWSTR::null(),
            &startup,
            &mut created,
        )
        .context("criar o processo do agente")?;
        drop(Owned(created.hThread));
        Ok(Owned(created.hProcess))
    }
}
