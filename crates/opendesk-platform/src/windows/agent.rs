//! Agente privilegiado: a parte do OpenDesk que enxerga e clica em tudo.
//!
//! O Windows impede um programa comum de injetar entrada em janelas de
//! administrador (UIPI) e de ver ou tocar a área de trabalho segura, onde
//! aparecem o aviso do UAC e a tela de bloqueio. Só um processo SYSTEM dentro
//! da sessão do usuário consegue. O agente é esse processo: o serviço do
//! OpenDesk o inicia, e ele faz **só** captura de tela e injeção de entrada, a
//! pedido do aplicativo, que continua rodando sem privilégio com a rede, a
//! interface e as chaves.
//!
//! A conversa é por um pipe nomeado, com os quadros em memória compartilhada
//! (um quadro 4K tem 33 MB; mandá-lo pelo pipe custaria duas cópias). Cada
//! função usa uma conexão própria, para que um clique nunca espere um quadro.
//!
//! Quem pode usar o agente:
//!
//! * o pipe só admite o SYSTEM e o usuário da sessão;
//! * o processo que conecta precisa ser o mesmo executável do agente, que
//!   fica numa pasta em que só administradores escrevem;
//! * e precisa provar isso lendo um segredo que o agente entrega dentro desse
//!   processo (um handle duplicado para ele), o que dispensa confiar no
//!   número de processo informado pelo pipe.
//!
//! O que isso **não** impede: um programa do mesmo usuário que injete código
//! no próprio OpenDesk herda o acesso ao agente. É o limite de todo programa
//! de acesso remoto cuja interface roda como o usuário; por isso o serviço é
//! opcional.

use super::capture::Capturer;
use super::input::Injector;
use super::{Owned, wide};
use crate::capture::{DisplayInfo, Frame, FrameStore};
use anyhow::{Context, Result, anyhow, bail, ensure};
use opendesk_proto::control::InputEvent;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{Read, Write};
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::sync::Arc;
use std::time::Duration;
use windows::Win32::Foundation::{
    CloseHandle, DUPLICATE_HANDLE_OPTIONS, DuplicateHandle, ERROR_PIPE_CONNECTED, GENERIC_ALL, HANDLE, HLOCAL,
    INVALID_HANDLE_VALUE, LocalFree,
};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo, SDDL_REVISION_1,
    SE_KERNEL_OBJECT,
};
use windows::Win32::Security::Cryptography::{BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom};
use windows::Win32::Security::{
    GetTokenInformation, IsWellKnownSid, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES,
    TOKEN_USER, TokenUser, WinLocalSystemSid,
};
use windows::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX};
use windows::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_ALL_ACCESS, FILE_MAP_READ, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile, PAGE_READWRITE,
    UnmapViewOfFile,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeClientProcessId, GetNamedPipeClientSessionId, PIPE_READMODE_BYTE,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};
use windows::Win32::System::RemoteDesktop::{ProcessIdToSessionId, WTSQueryUserToken};
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS, GetThreadDesktop, GetUserObjectInformationW, HDESK,
    OpenInputDesktop, SetThreadDesktop, UOI_NAME,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentProcessId, GetCurrentThreadId, INFINITE, OpenProcess, PROCESS_DUP_HANDLE,
    PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, QueryFullProcessImageNameW,
    WaitForSingleObject,
};
use windows::core::{PCWSTR, PWSTR};

const SECRET_LEN: usize = 32;
const CHALLENGE_BYTES: usize = 4096;
/// Nenhuma mensagem de controle chega perto disto; os quadros não passam pelo pipe.
const MAX_MESSAGE: usize = 64 * 1024;
const PIPE_BUFFER: u32 = 64 * 1024;
const ERROR_PIPE_BUSY: i32 = 231;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WireDisplay {
    name: String,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    primary: bool,
}

impl From<&DisplayInfo> for WireDisplay {
    fn from(d: &DisplayInfo) -> Self {
        Self { name: d.name.clone(), x: d.x, y: d.y, width: d.width, height: d.height, primary: d.primary }
    }
}

impl From<WireDisplay> for DisplayInfo {
    fn from(d: WireDisplay) -> Self {
        Self { name: d.name, x: d.x, y: d.y, width: d.width, height: d.height, primary: d.primary }
    }
}

#[derive(Serialize, Deserialize)]
enum Request {
    /// A resposta ao desafio: o segredo lido da memória entregue pelo agente.
    Proof([u8; SECRET_LEN]),
    /// Esta conexão vai pedir quadros do monitor `display`.
    Capture {
        display: u32,
    },
    Frame {
        timeout_ms: u32,
    },
    /// Esta conexão vai mandar entrada para o monitor `display`.
    Input {
        display: WireDisplay,
    },
    Event(InputEvent),
    ReleaseAll,
}

#[derive(Serialize, Deserialize)]
enum Reply {
    /// Handle, já dentro do processo do cliente, da memória com o segredo.
    Challenge {
        mapping: u64,
    },
    Denied(String),
    Capturing(WireDisplay),
    Injecting,
    Frame(Option<WireFrame>),
    Failed(String),
}

#[derive(Serialize, Deserialize)]
struct WireFrame {
    width: u32,
    height: u32,
    /// Presente quando a memória dos quadros foi (re)criada: handle no
    /// processo do cliente e tamanho em bytes.
    mapping: Option<(u64, u64)>,
}

fn write_msg<T: Serialize>(pipe: &mut File, msg: &T) -> Result<()> {
    let mut bytes = vec![0u8; 4];
    bytes = postcard::to_extend(msg, bytes).context("codificar mensagem do agente")?;
    let len = (bytes.len() - 4) as u32;
    bytes[..4].copy_from_slice(&len.to_le_bytes());
    pipe.write_all(&bytes).context("escrever no pipe do agente")
}

fn read_msg<T: DeserializeOwned>(pipe: &mut File) -> Result<T> {
    let mut len = [0u8; 4];
    pipe.read_exact(&mut len).context("ler do pipe do agente")?;
    let len = u32::from_le_bytes(len) as usize;
    ensure!(len <= MAX_MESSAGE, "mensagem do agente grande demais ({len} bytes)");
    let mut bytes = vec![0u8; len];
    pipe.read_exact(&mut bytes).context("ler do pipe do agente")?;
    postcard::from_bytes(&bytes).context("mensagem do agente inválida")
}

/// Um bloco de memória compartilhada entre o agente e o aplicativo.
struct Mapping {
    handle: Owned,
    view: *mut u8,
    len: usize,
}

impl Mapping {
    fn create(len: usize) -> Result<Self> {
        let handle = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                None,
                PAGE_READWRITE,
                (len as u64 >> 32) as u32,
                len as u32,
                PCWSTR::null(),
            )
        }
        .context("criar memória compartilhada")?;
        Self::map(Owned(handle), len, true)
    }

    /// Mapeia um handle que o outro lado duplicou para este processo.
    fn adopt(handle: u64, len: usize) -> Result<Self> {
        Self::map(Owned(HANDLE(handle as usize as *mut _)), len, false)
    }

    fn map(handle: Owned, len: usize, writable: bool) -> Result<Self> {
        let access = if writable { FILE_MAP_ALL_ACCESS } else { FILE_MAP_READ };
        let view = unsafe { MapViewOfFile(handle.0, access, 0, 0, len) };
        ensure!(!view.Value.is_null(), "mapear memória compartilhada: {}", windows::core::Error::from_thread());
        Ok(Self { handle, view: view.Value.cast(), len })
    }

    /// Entrega ao processo `process` um handle só de leitura desta memória.
    fn share_with(&self, process: HANDLE) -> Result<u64> {
        let mut remote = HANDLE::default();
        unsafe {
            DuplicateHandle(
                GetCurrentProcess(),
                self.handle.0,
                process,
                &mut remote,
                FILE_MAP_READ.0,
                false,
                DUPLICATE_HANDLE_OPTIONS(0),
            )
        }
        .context("entregar a memória compartilhada ao aplicativo")?;
        Ok(remote.0 as usize as u64)
    }

    fn bytes(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.view, self.len) }
    }

    fn bytes_mut(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.view, self.len) }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        unsafe {
            let _ = UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: self.view.cast() });
        }
    }
}

/// O nome do pipe do agente da sessão `session`.
fn pipe_name(session: u32) -> String {
    format!(r"\\.\pipe\OpenDesk.Agent.{session}")
}

fn current_session() -> Result<u32> {
    let mut session = 0;
    unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) }.context("descobrir a sessão do Windows")?;
    Ok(session)
}

fn process_image(process: HANDLE) -> Result<String> {
    let mut buffer = [0u16; 1024];
    let mut len = buffer.len() as u32;
    unsafe { QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, PWSTR(buffer.as_mut_ptr()), &mut len) }
        .context("descobrir o executável do processo")?;
    Ok(String::from_utf16_lossy(&buffer[..len as usize]))
}

/// O caminho deste executável, na mesma forma em que o Windows informa o de
/// outro processo (para a comparação entre os dois não depender de grafia).
pub(super) fn own_image() -> Result<String> {
    process_image(unsafe { GetCurrentProcess() })
}

// ───────────────────────────── Servidor ─────────────────────────────

struct Agent {
    pipe: String,
    /// Quem pode abrir o pipe. `None`: a segurança padrão (só em testes).
    sddl: Option<String>,
    /// O único executável aceito como cliente. `None`: qualquer um (testes).
    client_image: Option<String>,
}

/// Ponto de entrada do processo `--agent`. Só funciona iniciado pelo serviço:
/// fora dele não há privilégio para descobrir o usuário da sessão, e um agente
/// sem privilégio não serviria para nada.
pub fn run(parent: Option<u32>) -> Result<()> {
    super::init_process();
    let session = current_session()?;
    let user = session_user_sid(session).context("o agente só funciona iniciado pelo serviço do OpenDesk")?;
    if let Some(parent) = parent {
        exit_with(parent);
    }
    tracing::info!("agente no ar na sessão {session}");
    serve(Agent {
        pipe: pipe_name(session),
        // Dono SYSTEM (é o que o aplicativo confere antes de confiar no pipe);
        // acesso total para o SYSTEM, leitura e escrita para o usuário da sessão.
        sddl: Some(format!("O:SYG:SYD:P(A;;GA;;;SY)(A;;GRGW;;;{user})")),
        client_image: Some(own_image()?),
    })
}

/// Um agente sem as verificações e sem privilégio, para os testes exercitarem
/// o protocolo, a memória compartilhada, a captura e a entrada.
#[doc(hidden)]
pub fn serve_unverified(pipe: &str) -> Result<()> {
    serve(Agent { pipe: pipe.to_string(), sddl: None, client_image: None })
}

/// Encerra o agente quando o serviço que o criou deixar de existir.
fn exit_with(parent: u32) {
    let Ok(process) = (unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, parent) }) else {
        tracing::warn!("serviço {parent} não encontrado; o agente segue sem vigiá-lo");
        return;
    };
    let process = Owned(process);
    std::thread::spawn(move || {
        // Captura o handle inteiro (e não só o campo), que é o que pode mudar de thread.
        let process = process;
        unsafe { WaitForSingleObject(process.0, INFINITE) };
        std::process::exit(0);
    });
}

/// O SID do usuário que está na sessão, como texto (`S-1-5-21-…`).
pub(super) fn session_user_sid(session: u32) -> Result<String> {
    let mut token = HANDLE::default();
    unsafe { WTSQueryUserToken(session, &mut token) }.context("não há usuário nesta sessão")?;
    let token = Owned(token);
    let mut buffer = [0u64; 32];
    let mut needed = 0;
    unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            Some(buffer.as_mut_ptr().cast()),
            size_of_val(&buffer) as u32,
            &mut needed,
        )
    }
    .context("ler o usuário da sessão")?;
    let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
    let mut text = PWSTR::null();
    unsafe { ConvertSidToStringSidW(user.User.Sid, &mut text) }.context("converter o SID do usuário")?;
    let sid = unsafe { text.to_string() }.context("SID inválido");
    unsafe { LocalFree(Some(HLOCAL(text.0.cast()))) };
    sid
}

struct SecurityDescriptor(PSECURITY_DESCRIPTOR);

impl SecurityDescriptor {
    fn from_sddl(sddl: &str) -> Result<Self> {
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(wide(sddl).as_ptr()),
                SDDL_REVISION_1,
                &mut descriptor,
                None,
            )
        }
        .context("montar a segurança do pipe do agente")?;
        Ok(Self(descriptor))
    }
}

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        unsafe { LocalFree(Some(HLOCAL(self.0.0))) };
    }
}

fn serve(agent: Agent) -> Result<()> {
    let name = wide(&agent.pipe);
    let descriptor = agent.sddl.as_deref().map(SecurityDescriptor::from_sddl).transpose()?;
    let attributes = descriptor.as_ref().map(|descriptor| SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0.0,
        bInheritHandle: false.into(),
    });
    let agent = Arc::new(agent);
    let mut first = true;
    loop {
        // Na primeira instância exige ser o criador do nome: se outro processo
        // já tiver um pipe com este nome, o agente prefere não subir.
        let mode = if first { PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE } else { PIPE_ACCESS_DUPLEX };
        let pipe = unsafe {
            CreateNamedPipeW(
                PCWSTR(name.as_ptr()),
                mode,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                PIPE_BUFFER,
                PIPE_BUFFER,
                0,
                attributes.as_ref().map(|attributes| attributes as *const _),
            )
        };
        if pipe.is_invalid() {
            return Err(anyhow!(windows::core::Error::from_thread()).context(format!("criar o pipe {}", agent.pipe)));
        }
        first = false;
        if let Err(err) = unsafe { ConnectNamedPipe(pipe, None) }
            && err.code() != ERROR_PIPE_CONNECTED.to_hresult()
        {
            tracing::debug!("conexão ao agente falhou: {err}");
            unsafe {
                let _ = CloseHandle(pipe);
            }
            continue;
        }
        let pipe = unsafe { File::from_raw_handle(pipe.0) };
        let agent = agent.clone();
        std::thread::spawn(move || {
            if let Err(err) = attend(pipe, &agent) {
                tracing::debug!("conexão ao agente encerrada: {err:#}");
            }
        });
    }
}

/// Atende uma conexão: confere quem é, e passa a capturar ou a injetar.
fn attend(mut pipe: File, agent: &Agent) -> Result<()> {
    let handle = HANDLE(pipe.as_raw_handle());
    let mut pid = 0;
    unsafe { GetNamedPipeClientProcessId(handle, &mut pid) }.context("identificar o cliente do pipe")?;
    let client = Owned(
        unsafe { OpenProcess(PROCESS_DUP_HANDLE | PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }
            .context("abrir o processo do cliente")?,
    );
    if let Some(expected) = &agent.client_image {
        let image = process_image(client.0)?;
        let mut session = 0;
        unsafe { GetNamedPipeClientSessionId(handle, &mut session) }.context("sessão do cliente")?;
        if !image.eq_ignore_ascii_case(expected) || session != current_session()? {
            let why = "Só o OpenDesk instalado pode usar o controle total.".to_string();
            let _ = write_msg(&mut pipe, &Reply::Denied(why));
            bail!("cliente recusado: {image} (sessão {session})");
        }
    }

    // O segredo vai para dentro do processo conferido acima. Quem não for
    // esse processo não tem como lê-lo, mesmo que tenha mentido o PID.
    let mut challenge = Mapping::create(CHALLENGE_BYTES)?;
    let mut secret = [0u8; SECRET_LEN];
    unsafe { BCryptGenRandom(None, &mut secret, BCRYPT_USE_SYSTEM_PREFERRED_RNG) }.ok().context("gerar segredo")?;
    challenge.bytes_mut()[..SECRET_LEN].copy_from_slice(&secret);
    write_msg(&mut pipe, &Reply::Challenge { mapping: challenge.share_with(client.0)? })?;
    match read_msg(&mut pipe)? {
        Request::Proof(proof) if proof == secret => {}
        _ => {
            let _ = write_msg(&mut pipe, &Reply::Denied("Prova de identidade inválida.".into()));
            bail!("cliente não provou a identidade");
        }
    }
    drop(challenge);

    match read_msg(&mut pipe)? {
        Request::Capture { display } => capture(pipe, client, display as usize),
        Request::Input { display } => inject(pipe, display.into()),
        _ => bail!("pedido inesperado no início da conexão"),
    }
}

fn capture(mut pipe: File, client: Owned, display: usize) -> Result<()> {
    let mut desktop = InputDesktop::new();
    desktop.follow();
    let mut capturer = match Capturer::new(display) {
        Ok(capturer) => capturer,
        Err(err) => {
            write_msg(&mut pipe, &Reply::Failed(format!("{err:#}")))?;
            return Err(err);
        }
    };
    write_msg(&mut pipe, &Reply::Capturing(capturer.display().into()))?;
    tracing::info!("capturando {} na área de trabalho \"{}\"", capturer.display().name, desktop.name);
    let mut shared = SharedFrames { client, mapping: None, fresh: false };
    loop {
        let Request::Frame { timeout_ms } = read_msg(&mut pipe)? else { bail!("pedido inesperado na captura") };
        // O UAC e a tela de bloqueio ficam em outra área de trabalho. A thread
        // precisa se mudar para lá antes de a duplicação poder ser refeita.
        if desktop.follow() {
            capturer.reset();
        }
        let reply = match capturer.frame_into(timeout_ms.min(1000), &mut shared) {
            Ok(Some((width, height))) => Reply::Frame(Some(WireFrame {
                width: width as u32,
                height: height as u32,
                mapping: shared.announce()?,
            })),
            Ok(None) => Reply::Frame(None),
            Err(err) => Reply::Failed(format!("{err:#}")),
        };
        write_msg(&mut pipe, &reply)?;
    }
}

fn inject(mut pipe: File, display: DisplayInfo) -> Result<()> {
    let mut desktop = InputDesktop::new();
    desktop.follow();
    let mut injector = Injector::new(&display);
    write_msg(&mut pipe, &Reply::Injecting)?;
    let (monitor, area) = (&display.name, &desktop.name);
    tracing::info!("recebendo entrada para {monitor} na área de trabalho \"{area}\"");
    loop {
        match read_msg(&mut pipe)? {
            Request::Event(event) => {
                // O SendInput só vale na área de trabalho em que a thread está.
                desktop.follow();
                injector.apply(event);
            }
            Request::ReleaseAll => injector.release_all(),
            _ => bail!("pedido inesperado na entrada"),
        }
    }
}

/// A memória onde o agente captura e de onde o aplicativo lê os quadros.
struct SharedFrames {
    client: Owned,
    mapping: Option<Mapping>,
    /// A memória atual ainda não foi entregue ao aplicativo.
    fresh: bool,
}

impl SharedFrames {
    fn announce(&mut self) -> Result<Option<(u64, u64)>> {
        if !std::mem::take(&mut self.fresh) {
            return Ok(None);
        }
        let mapping = self.mapping.as_ref().expect("memória criada antes de anunciar");
        Ok(Some((mapping.share_with(self.client.0)?, mapping.len as u64)))
    }
}

impl FrameStore for SharedFrames {
    fn reserve(&mut self, len: usize) -> Result<&mut [u8]> {
        if self.mapping.as_ref().is_none_or(|mapping| mapping.len < len) {
            // Só acontece no primeiro quadro e quando a resolução aumenta.
            self.mapping = None;
            self.mapping = Some(Mapping::create(len.next_multiple_of(64 * 1024))?);
            self.fresh = true;
        }
        Ok(&mut self.mapping.as_mut().expect("criada acima").bytes_mut()[..len])
    }
}

/// Mantém a thread na área de trabalho que está recebendo entrada: a comum,
/// a do UAC ou a da tela de bloqueio.
struct InputDesktop {
    name: String,
    held: Option<HDESK>,
}

impl InputDesktop {
    fn new() -> Self {
        let name = unsafe { GetThreadDesktop(GetCurrentThreadId()) }.map(desktop_name).unwrap_or_default();
        Self { name, held: None }
    }

    /// Muda a thread para a área de trabalho ativa, se ela mudou. Devolve se
    /// houve mudança.
    fn follow(&mut self) -> bool {
        let Ok(desktop) =
            (unsafe { OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_ACCESS_FLAGS(GENERIC_ALL.0)) })
        else {
            return false;
        };
        let name = desktop_name(desktop);
        if name == self.name || unsafe { SetThreadDesktop(desktop) }.is_err() {
            unsafe {
                let _ = CloseDesktop(desktop);
            }
            return false;
        }
        tracing::info!("área de trabalho ativa: {name}");
        self.name = name;
        if let Some(previous) = self.held.replace(desktop) {
            unsafe {
                let _ = CloseDesktop(previous);
            }
        }
        true
    }
}

impl Drop for InputDesktop {
    fn drop(&mut self) {
        if let Some(desktop) = self.held.take() {
            unsafe {
                let _ = CloseDesktop(desktop);
            }
        }
    }
}

fn desktop_name(desktop: HDESK) -> String {
    let mut buffer = [0u16; 128];
    let mut needed = 0;
    let read = unsafe {
        GetUserObjectInformationW(
            HANDLE(desktop.0),
            UOI_NAME,
            Some(buffer.as_mut_ptr().cast()),
            size_of_val(&buffer) as u32,
            Some(&mut needed),
        )
    };
    if read.is_err() {
        return String::new();
    }
    let len = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..len])
}

// ───────────────────────────── Cliente ─────────────────────────────

/// Conecta ao agente e prova a identidade. Com `verify`, só aceita um pipe
/// criado pelo SYSTEM: outro programa poderia ter criado um com o mesmo nome
/// para ver as teclas digitadas na sessão.
fn connect(pipe: &str, verify: bool) -> Result<File> {
    let mut attempts = 0;
    let mut file = loop {
        match File::options().read(true).write(true).open(pipe) {
            Ok(file) => break file,
            // Todas as instâncias ocupadas: o agente cria outra em seguida.
            Err(err) if err.raw_os_error() == Some(ERROR_PIPE_BUSY) && attempts < 20 => {
                attempts += 1;
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(err) => return Err(anyhow!(err).context("o agente do OpenDesk não está no ar")),
        }
    };
    if verify {
        ensure!(owned_by_system(&file)?, "o pipe do agente não pertence ao sistema");
    }
    match read_msg(&mut file)? {
        Reply::Challenge { mapping } => {
            let challenge = Mapping::adopt(mapping, CHALLENGE_BYTES)?;
            let mut secret = [0u8; SECRET_LEN];
            secret.copy_from_slice(&challenge.bytes()[..SECRET_LEN]);
            write_msg(&mut file, &Request::Proof(secret))?;
        }
        Reply::Denied(why) => bail!(why),
        _ => bail!("resposta inesperada do agente"),
    }
    Ok(file)
}

fn owned_by_system(pipe: &File) -> Result<bool> {
    let mut owner = PSID::default();
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    unsafe {
        GetSecurityInfo(
            HANDLE(pipe.as_raw_handle()),
            SE_KERNEL_OBJECT,
            OWNER_SECURITY_INFORMATION,
            Some(&mut owner),
            None,
            None,
            None,
            Some(&mut descriptor),
        )
    }
    .ok()
    .context("ler o dono do pipe do agente")?;
    let system = unsafe { IsWellKnownSid(owner, WinLocalSystemSid) }.as_bool();
    unsafe { LocalFree(Some(HLOCAL(descriptor.0))) };
    Ok(system)
}

/// O agente desta sessão está no ar e aceita este executável?
pub fn available() -> bool {
    current_session().and_then(|session| connect(&pipe_name(session), true)).is_ok()
}

/// Captura de tela feita pelo agente. Mesma interface do `Capturer`.
pub struct RemoteCapturer {
    pipe: File,
    info: DisplayInfo,
    frames: Option<Mapping>,
    /// Largura e altura do quadro que está na memória compartilhada.
    size: (usize, usize),
}

impl RemoteCapturer {
    pub fn open(display: usize) -> Result<Self> {
        Self::start(connect(&pipe_name(current_session()?), true)?, display)
    }

    #[doc(hidden)]
    pub fn open_unverified(pipe: &str, display: usize) -> Result<Self> {
        Self::start(connect(pipe, false)?, display)
    }

    fn start(mut pipe: File, display: usize) -> Result<Self> {
        write_msg(&mut pipe, &Request::Capture { display: display as u32 })?;
        match read_msg(&mut pipe)? {
            Reply::Capturing(info) => Ok(Self { pipe, info: info.into(), frames: None, size: (0, 0) }),
            Reply::Failed(why) => bail!(why),
            _ => bail!("resposta inesperada do agente"),
        }
    }

    pub fn display(&self) -> &DisplayInfo {
        &self.info
    }

    /// Espera até `timeout_ms` por uma mudança na tela. `Ok(None)` = nada mudou.
    pub fn frame(&mut self, timeout_ms: u32) -> Result<Option<Frame<'_>>> {
        Ok(if self.advance(timeout_ms)? { Some(self.current()) } else { None })
    }

    /// Pede o próximo quadro. `true` quando há um novo em [`current`](Self::current).
    /// Separado de `frame` para quem precisa tratar a falha antes de pegar
    /// os pixels emprestados.
    pub fn advance(&mut self, timeout_ms: u32) -> Result<bool> {
        write_msg(&mut self.pipe, &Request::Frame { timeout_ms })?;
        let frame = match read_msg(&mut self.pipe)? {
            Reply::Frame(Some(frame)) => frame,
            Reply::Frame(None) => return Ok(false),
            Reply::Failed(why) => bail!(why),
            _ => bail!("resposta inesperada do agente"),
        };
        if let Some((handle, len)) = frame.mapping {
            self.frames = None;
            self.frames = Some(Mapping::adopt(handle, len as usize)?);
        }
        let frames = self.frames.as_ref().context("o agente não entregou a memória dos quadros")?;
        let (width, height) = (frame.width as usize, frame.height as usize);
        let len = width.checked_mul(height).and_then(|pixels| pixels.checked_mul(4));
        ensure!(len.is_some_and(|len| len <= frames.len), "quadro do agente maior que a memória");
        self.size = (width, height);
        Ok(true)
    }

    /// O quadro mais recente. Vale até o próximo `advance`.
    pub fn current(&self) -> Frame<'_> {
        let (width, height) = self.size;
        let data = self.frames.as_ref().map_or(&[][..], |frames| &frames.bytes()[..width * height * 4]);
        Frame { data, width, height, stride: width * 4 }
    }
}

/// Entrada injetada pelo agente. Mesma interface do `Injector`.
pub struct RemoteInjector {
    pipe: File,
}

impl RemoteInjector {
    pub fn open(display: &DisplayInfo) -> Result<Self> {
        Self::start(connect(&pipe_name(current_session()?), true)?, display)
    }

    #[doc(hidden)]
    pub fn open_unverified(pipe: &str, display: &DisplayInfo) -> Result<Self> {
        Self::start(connect(pipe, false)?, display)
    }

    fn start(mut pipe: File, display: &DisplayInfo) -> Result<Self> {
        write_msg(&mut pipe, &Request::Input { display: display.into() })?;
        match read_msg(&mut pipe)? {
            Reply::Injecting => Ok(Self { pipe }),
            _ => bail!("resposta inesperada do agente"),
        }
    }

    pub fn apply(&mut self, event: InputEvent) -> Result<()> {
        write_msg(&mut self.pipe, &Request::Event(event))
    }

    pub fn release_all(&mut self) -> Result<()> {
        write_msg(&mut self.pipe, &Request::ReleaseAll)
    }
}
