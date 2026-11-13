//! Estado e telas do aplicativo.
//!
//! Uma janela só e enxuta: a tela inicial (o ID deste computador ao lado do
//! campo para acessar outro) dá lugar à visão remota, em janela maximizada,
//! quando uma sessão de saída é autorizada. Pedidos de entrada aparecem como
//! diálogo modal por cima de qualquer tela, e uma sessão de entrada ativa fica
//! sempre anunciada numa faixa no topo.

mod settings;

use crate::config::{Entry, History, Kind, Recents, Settings};
use crate::theme::{self, Palette};
use anyhow::{Context as _, Result};
use eframe::egui::{
    self, Align, Color32, ColorImage, CornerRadius, Event, Layout, Margin, MouseWheelUnit, PointerButton, Pos2, Rect,
    RichText, Sense, Stroke, TextureHandle, TextureOptions, Vec2,
};
use opendesk_net::{DeviceId, Fingerprint, Identity, Nat, Node, Presence};
use opendesk_platform::keyboard::KeyboardGrab;
use opendesk_proto::control::{InputEvent, MouseButton, Permissions};
use opendesk_session::Wake;
use opendesk_session::auth::PasswordAccess;
use opendesk_session::backend::{Clipboard as _, SystemBackend, SystemClipboard};
use opendesk_session::files::{FileEvent, FileState};
use opendesk_session::host::{Host, HostEvent, PeerInfo, Unattended};
use opendesk_session::viewer::{InputSender, Lookup, Route, Target, Viewer, ViewerEvent};
use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};

/// O host desiste do pedido depois deste tempo (ver `opendesk-session`).
const REQUEST_TTL: Duration = Duration::from_secs(60);

/// Tamanho da janela na tela inicial.
pub const HOME_SIZE: [f32; 2] = [940.0, 600.0];
/// Largura do conteúdo da coluna "este computador".
const SIDE_WIDTH: f32 = 264.0;
/// Largura máxima do conteúdo da área principal. Formulário, cabeçalho e
/// linhas de configuração terminam todos nesta mesma borda direita.
const CONTENT_WIDTH: f32 = 560.0;
/// Altura do símbolo da marca e o respiro abaixo dele. A coluna da direita
/// desce o mesmo tanto, para os rótulos das duas colunas ficarem alinhados.
const LOGO_HEIGHT: f32 = 30.0;
const LOGO_GAP: f32 = 36.0;
/// Largura do botão ao lado do campo (igual para "Conectar" e "Cancelar",
/// para o campo não mudar de tamanho entre os estados).
const ACTION_WIDTH: f32 = 104.0;

/// O que a área principal mostra.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Screen {
    Home,
    Settings(Tab),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    General,
    Security,
    Network,
    History,
    About,
}

/// O nome com que este computador se apresenta: o das configurações ou, se
/// estiver vazio, o do sistema.
fn effective_name(settings: &Settings, system_name: &str) -> String {
    let name = settings.device_name.trim();
    if name.is_empty() { system_name.to_string() } else { name.to_string() }
}

pub struct App {
    runtime: tokio::runtime::Runtime,
    node: Arc<Node>,
    host: Arc<Host>,
    host_events: mpsc::UnboundedReceiver<HostEvent>,
    wake: Wake,
    ctx: egui::Context,
    logo: theme::Logo,
    data_dir: PathBuf,
    settings: Settings,
    /// Um problema ao gravar ou aplicar configurações, mostrado na tela delas.
    settings_problem: Option<String>,
    net: settings::NetForm,
    /// A senha de acesso não supervisionado, na forma guardada (ou nenhuma).
    access: Option<PasswordAccess>,
    /// Senha nova sendo digitada nas configurações.
    new_password: String,
    /// Resultado da última tentativa de salvar a senha: (deu certo, texto).
    password_note: Option<(bool, String)>,
    /// Se o OpenDesk está registrado para abrir com o sistema.
    autostart: bool,
    recents: Recents,
    history: History,
    /// Os eventos mais recentes do histórico, para a tela de configurações.
    history_view: Vec<Entry>,
    screen: Screen,
    /// Nome do computador no sistema; vale quando o das configurações está vazio.
    system_name: String,
    device_name: String,
    local_address: String,
    /// Busca por ID; `None` quando não há servidor de encontro configurado.
    lookup: Option<Arc<Lookup>>,
    presence: Arc<Mutex<Presence>>,
    presence_task: Option<tokio::task::JoinHandle<()>>,

    requests: Vec<Request>,
    inbound: Vec<Inbound>,

    address: String,
    want_control: bool,
    outgoing: Option<Outgoing>,
    notice: Option<String>,
    copied_at: Option<Instant>,
    /// A janela foi maximizada pelo app ao entrar na sessão (e não pela pessoa).
    maximized_for_session: bool,
    /// Arquivos em andamento ou recém-terminados, das duas direções.
    transfers: Vec<Transfer>,
    /// Um seletor de arquivos aberto: para qual sessão é e por onde chega a
    /// escolha. Roda numa thread própria para não travar a interface.
    picker: Option<(Option<u64>, std::sync::mpsc::Receiver<Vec<PathBuf>>)>,
    /// Onde ficam os arquivos recebidos.
    files_dir: PathBuf,
    /// A sessão está em tela cheia; a barra só aparece com o ponteiro no topo.
    fullscreen: bool,
    bar_until: Option<Instant>,
    /// Em testes (`OPENDESK_WINDOW_POS`), a janela nunca toma o foco nem se maximiza.
    quiet: bool,
    #[cfg(debug_assertions)]
    preview_details: std::cell::Cell<bool>,

    keyboard: Option<KeyboardGrab>,
    key_target: Arc<Mutex<Option<InputSender>>>,
}

struct Request {
    session: u64,
    peer: PeerInfo,
    reply: oneshot::Sender<Option<Permissions>>,
    since: Instant,
}

struct Inbound {
    session: u64,
    peer: PeerInfo,
    permissions: Permissions,
    since: Instant,
    /// Entrou pela senha de acesso não supervisionado.
    by_password: bool,
}

/// Um arquivo sendo trocado numa sessão.
struct Transfer {
    /// A sessão de entrada a que pertence; `None` é a sessão em que este
    /// computador acessa outro.
    session: Option<u64>,
    /// Com quem é a troca.
    peer: String,
    event: FileEvent,
    updated: Instant,
}

impl Transfer {
    fn finished(&self) -> bool {
        matches!(self.event.state, FileState::Done(_) | FileState::Refused | FileState::Failed(_))
    }

    /// O que dizer sobre este arquivo, numa linha.
    fn describe(&self) -> String {
        let (name, event) = (&self.event.name, &self.event);
        let percent = |done: &u64| done.saturating_mul(100).checked_div(event.size).unwrap_or(100);
        match (&event.state, event.incoming) {
            (FileState::Offered, _) => {
                format!("{} quer enviar {name} ({})", self.peer, crate::config::file_size(event.size))
            }
            (FileState::Progress(done), true) => format!("Recebendo {name}, {}%", percent(done)),
            (FileState::Progress(done), false) => format!("Enviando {name}, {}%", percent(done)),
            (FileState::Done(_), true) => format!("Recebido: {name}"),
            (FileState::Done(_), false) => format!("Enviado: {name}"),
            (FileState::Refused, true) => format!("{name}: recusado"),
            (FileState::Refused, false) => format!("{name}: {} não aceitou o arquivo", self.peer),
            (FileState::Failed(why), _) => format!("{name}: {why}"),
        }
    }
}

/// O que a pessoa pediu numa linha de transferência.
enum TransferAction {
    Answer(u64, bool),
    OpenFolder,
}

/// Como está a tentativa de entrar com a senha do computador remoto.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PasswordTry {
    Idle,
    Checking,
    Wrong { retry: bool },
}

enum Stage {
    Connecting,
    Waiting,
    Live { permissions: Permissions },
}

struct Outgoing {
    viewer: Viewer,
    events: mpsc::UnboundedReceiver<ViewerEvent>,
    target: String,
    by_id: bool,
    host_name: String,
    fingerprint: Option<Fingerprint>,
    route: Option<Route>,
    stage: Stage,
    /// Quando a sessão foi autorizada, para o histórico.
    live_since: Option<Instant>,
    /// O host aceita senha de acesso não supervisionado.
    accepts_password: bool,
    password: String,
    password_try: PasswordTry,
    /// Liberada pela senha (para o histórico).
    by_password: bool,
    /// Qual monitor do host está na tela, e quantos ele tem.
    display_index: u8,
    displays: u8,
    /// Mouse e teclado não são enviados enquanto pausado.
    paused: bool,
    /// Presente quando a sessão sincroniza a área de transferência.
    clipboard: Option<SystemClipboard>,
    texture: Option<TextureHandle>,
    epoch: u64,
    size: [usize; 2],
    buttons: HashSet<MouseButton>,
    stats: Stats,
}

struct Stats {
    at: Instant,
    frames: u64,
    bytes: u64,
    fps: f32,
    mbps: f32,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Result<Self> {
        let data_dir: PathBuf = match std::env::var_os("OPENDESK_DATA_DIR") {
            Some(dir) => dir.into(),
            None => directories::ProjectDirs::from("org", "OpenDesk", "OpenDesk")
                .context("não foi possível localizar a pasta de dados do usuário")?
                .data_dir()
                .to_path_buf(),
        };
        let identity = Arc::new(Identity::load_or_create(&data_dir)?);
        let settings = Settings::load(&data_dir);
        // O tema das configurações vale primeiro; `OPENDESK_THEME`, se
        // definida, tem a última palavra (ver `theme::install`).
        cc.egui_ctx.set_theme(match settings.theme {
            crate::config::ThemeChoice::System => egui::ThemePreference::System,
            crate::config::ThemeChoice::Light => egui::ThemePreference::Light,
            crate::config::ThemeChoice::Dark => egui::ThemePreference::Dark,
        });
        theme::install(&cc.egui_ctx);
        let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().context("iniciar runtime")?;

        let port = settings.port();
        let node = {
            let _guard = runtime.enter();
            let any = IpAddr::V4(Ipv4Addr::UNSPECIFIED);
            // Se a porta padrão estiver ocupada (outra instância), usa uma livre.
            Node::bind(identity.clone(), SocketAddr::new(any, port))
                .or_else(|_| Node::bind(identity.clone(), SocketAddr::new(any, 0)))?
        };
        let node = Arc::new(node);
        let bound_port = node.local_addr()?.port();
        let local_address = match opendesk_net::local_ip() {
            Some(ip) => format!("{ip}:{bound_port}"),
            None => format!("porta {bound_port} (sem rede)"),
        };

        let ctx = cc.egui_ctx.clone();
        let wake: Wake = Arc::new(move || ctx.request_repaint());
        let system_name = std::env::var("COMPUTERNAME")
            .or_else(|_| std::env::var("HOSTNAME"))
            .unwrap_or_else(|_| "Meu computador".into());
        let device_name = effective_name(&settings, &system_name);
        let (host, host_events) =
            Host::new(node.clone(), Arc::new(SystemBackend { display: 0 }), device_name.clone(), wake.clone());
        runtime.spawn(host.clone().serve());

        // As teclas saem direto da thread do gancho para a sessão, sem esperar
        // o próximo quadro da interface.
        let key_target: Arc<Mutex<Option<InputSender>>> = Arc::default();
        let keyboard = match KeyboardGrab::install() {
            Ok((grab, keys)) => {
                let target = key_target.clone();
                std::thread::Builder::new().name("opendesk-keys".into()).spawn(move || {
                    while let Ok(key) = keys.recv() {
                        if let Some(sender) = target.lock().unwrap().as_ref() {
                            sender.send(InputEvent::Key {
                                scancode: key.scancode,
                                extended: key.extended,
                                down: key.down,
                            });
                        }
                    }
                })?;
                Some(grab)
            }
            Err(err) => {
                tracing::warn!("captura de teclado indisponível: {err:#}");
                None
            }
        };

        // Arquivos recebidos vão para Downloads\OpenDesk (`OPENDESK_FILES_DIR`
        // troca a pasta, para testes e implantações).
        let files_dir: PathBuf = std::env::var_os("OPENDESK_FILES_DIR").map(PathBuf::from).unwrap_or_else(|| {
            directories::UserDirs::new()
                .and_then(|dirs| dirs.download_dir().map(|downloads| downloads.join("OpenDesk")))
                .unwrap_or_else(|| data_dir.join("Recebidos"))
        });
        host.set_files_dir(Some(files_dir.clone()));

        let mut app = Self {
            runtime,
            node,
            host,
            host_events,
            wake,
            ctx: cc.egui_ctx.clone(),
            logo: theme::Logo::load(),
            net: settings::NetForm::of(&settings),
            access: crate::config::load_access(&data_dir),
            new_password: String::new(),
            password_note: None,
            settings,
            settings_problem: None,
            autostart: opendesk_platform::system::autostart_command().is_some(),
            recents: Recents::load(&data_dir),
            history_view: History::new(&data_dir).latest(200),
            history: History::new(&data_dir),
            data_dir,
            screen: Screen::Home,
            system_name,
            device_name,
            local_address,
            lookup: None,
            presence: Arc::new(Mutex::new(Presence::Connecting)),
            presence_task: None,
            requests: Vec::new(),
            inbound: Vec::new(),
            address: String::new(),
            want_control: true,
            outgoing: None,
            notice: None,
            copied_at: None,
            maximized_for_session: false,
            transfers: Vec::new(),
            picker: None,
            files_dir: files_dir.clone(),
            fullscreen: false,
            bar_until: None,
            quiet: std::env::var_os("OPENDESK_WINDOW_POS").is_some(),
            #[cfg(debug_assertions)]
            preview_details: Default::default(),
            keyboard,
            key_target,
        };
        app.start_presence();
        app.apply_host_policy();
        #[cfg(debug_assertions)]
        app.preview();
        // `--minimized` é como o Windows abre o app junto com o sistema.
        if std::env::args().any(|arg| arg == "--minimized") {
            app.ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
        }
        // `opendesk <ID ou endereço>` conecta direto ao abrir.
        if let Some(target) = std::env::args().skip(1).find(|arg| !arg.starts_with("--")) {
            app.address = target;
            app.connect();
        }
        Ok(app)
    }

    /// (Re)inicia a presença no servidor de encontro conforme as configurações.
    /// Com um servidor, este computador fica acessível pelo ID; sem, só por
    /// endereço.
    fn start_presence(&mut self) {
        if let Some(task) = self.presence_task.take() {
            task.abort();
        }
        // Um estado novo a cada vez: a tarefa antiga não escreve mais nele.
        self.presence = Arc::new(Mutex::new(Presence::Connecting));
        self.lookup = self.settings.rendezvous_url().and_then(|url| {
            Lookup::new(&self.data_dir, &url, self.settings.relay())
                .map_err(|err| tracing::warn!("servidor de encontro mal configurado: {err:#}"))
                .ok()
        });
        let Some(lookup) = self.lookup.clone() else { return };
        let (node, presence, wake) = (self.node.clone(), self.presence.clone(), self.wake.clone());
        self.presence_task = Some(self.runtime.spawn(async move {
            let servers = lookup.stun_servers.clone();
            let update = move |now: Presence| {
                *presence.lock().unwrap() = now;
                wake();
            };
            lookup.rendezvous.serve(node, servers, lookup.relay.clone(), update).await;
        }));
    }

    /// Atualiza a lista de transferências com uma novidade, e põe no histórico
    /// os arquivos que terminaram.
    fn track(&mut self, session: Option<u64>, peer: &str, peer_id: String, event: FileEvent) {
        if matches!(event.state, FileState::Done(_)) {
            self.record(Entry {
                when: opendesk_platform::system::local_time(),
                incoming: event.incoming,
                kind: Kind::File,
                name: peer.to_string(),
                id: peer_id,
                control: false,
                seconds: 0,
                password: false,
                file: event.name.clone(),
            });
        }
        let (id, incoming) = (event.id, event.incoming);
        let same = |t: &Transfer| t.session == session && t.event.id == id && t.event.incoming == incoming;
        let transfer = Transfer { session, peer: peer.to_string(), event, updated: Instant::now() };
        match self.transfers.iter_mut().find(|t| same(t)) {
            Some(existing) => *existing = transfer,
            None => self.transfers.push(transfer),
        }
    }

    /// Envia o que saiu do seletor de arquivos ou foi arrastado para a
    /// janela, e tira da lista o que já terminou há um tempo.
    fn pump_files(&mut self, ctx: &egui::Context) {
        let picked = self.picker.as_ref().and_then(|(target, chosen)| match chosen.try_recv() {
            Ok(paths) => Some((*target, paths)),
            Err(std::sync::mpsc::TryRecvError::Disconnected) => Some((*target, Vec::new())),
            Err(std::sync::mpsc::TryRecvError::Empty) => None,
        });
        if let Some((target, paths)) = picked {
            self.picker = None;
            for path in paths {
                self.send_file(target, path);
            }
        }
        // Soltar arquivos sobre a tela remota envia para o computador remoto.
        let sharing =
            matches!(&self.outgoing, Some(Outgoing { stage: Stage::Live { permissions }, .. }) if permissions.files);
        if sharing {
            let dropped: Vec<PathBuf> = ctx.input(|i| {
                i.raw.dropped_files.iter().map(|file| file.path().to_path_buf()).filter(|path| path.is_file()).collect()
            });
            for path in dropped {
                self.send_file(None, path);
            }
        }
        // Um arquivo recebido fica mais tempo na tela: é ali que se abre a pasta.
        self.transfers.retain(|t| {
            let keep = if matches!(t.event.state, FileState::Done(_)) && t.event.incoming { 30 } else { 8 };
            !t.finished() || t.updated.elapsed() < Duration::from_secs(keep)
        });
        // Sessões que acabaram levam as suas linhas.
        let outgoing_alive = self.outgoing.is_some();
        let inbound = &self.inbound;
        self.transfers.retain(|t| match t.session {
            None => outgoing_alive,
            Some(session) => inbound.iter().any(|s| s.session == session),
        });
    }

    fn send_file(&mut self, target: Option<u64>, path: PathBuf) {
        match target {
            Some(session) => self.host.send_file(self.runtime.handle(), session, path),
            None => {
                if let Some(out) = &self.outgoing {
                    out.viewer.send_file(path);
                }
            }
        }
    }

    /// Abre o seletor de arquivos do sistema, numa thread própria: a janela
    /// continua viva e o teclado para de ir para o computador remoto assim que
    /// ela perde o foco.
    fn pick_files(&mut self, target: Option<u64>) {
        if self.picker.is_some() {
            return;
        }
        let (chosen, receiver) = std::sync::mpsc::channel();
        let wake = self.wake.clone();
        let spawned = std::thread::Builder::new().name("opendesk-arquivos".into()).spawn(move || {
            let paths = rfd::FileDialog::new().set_title("Enviar arquivos").pick_files().unwrap_or_default();
            let _ = chosen.send(paths);
            wake();
        });
        if spawned.is_ok() {
            self.picker = Some((target, receiver));
        }
    }

    /// As linhas de transferência de uma sessão, com as ações de cada uma.
    fn transfer_rows(&mut self, ui: &mut egui::Ui, session: Option<u64>, ink: Color32) {
        let mut action = None;
        // Linhas baixas e juntas: é uma lista de acompanhamento, não um formulário.
        let before = ui.spacing().clone();
        ui.spacing_mut().interact_size.y = 20.0;
        ui.spacing_mut().item_spacing.y = 3.0;
        for transfer in self.transfers.iter().filter(|t| t.session == session) {
            ui.horizontal(|ui| {
                ui.label(RichText::new(transfer.describe()).small().color(ink));
                match (&transfer.event.state, transfer.event.incoming) {
                    (FileState::Offered, _) => {
                        if ui.small_button("Receber").clicked() {
                            action = Some(TransferAction::Answer(transfer.event.id, true));
                        }
                        if ui.small_button("Recusar").clicked() {
                            action = Some(TransferAction::Answer(transfer.event.id, false));
                        }
                    }
                    (FileState::Done(_), true) if ui.small_button("Abrir a pasta").clicked() => {
                        action = Some(TransferAction::OpenFolder);
                    }
                    _ => {}
                }
            });
        }
        *ui.spacing_mut() = before;
        match action {
            Some(TransferAction::Answer(id, accept)) => {
                if let Some(out) = &self.outgoing {
                    out.viewer.answer_file(id, accept);
                }
            }
            Some(TransferAction::OpenFolder) => self.open_external(self.files_dir.clone().as_os_str()),
            None => {}
        }
    }

    /// Abre uma pasta ou um endereço da web no programa padrão do sistema.
    fn open_external(&mut self, target: &std::ffi::OsStr) {
        let opener = if cfg!(windows) { "explorer" } else { "xdg-open" };
        if let Err(err) = std::process::Command::new(opener).arg(target).spawn() {
            tracing::warn!("não foi possível abrir {}: {err}", target.to_string_lossy());
        }
    }

    /// O acesso não supervisionado está de fato valendo: ligado, com senha
    /// salva e com a entrada aceita.
    fn unattended_active(&self) -> bool {
        self.settings.accept_incoming && self.settings.unattended && self.access.is_some()
    }

    /// O máximo que uma sessão de entrada pode receber, pelas configurações.
    fn most_permissions(&self) -> Permissions {
        let control = self.settings.allow_control;
        Permissions {
            control,
            clipboard: control && self.settings.allow_clipboard,
            files: control && self.settings.allow_files,
        }
    }

    /// Passa ao host a política de acesso não supervisionado em vigor.
    fn apply_host_policy(&self) {
        let policy = self
            .access
            .clone()
            .filter(|_| self.unattended_active())
            .map(|access| Unattended { access, permissions: self.most_permissions() });
        self.host.set_unattended(policy);
    }

    /// Acrescenta um evento ao histórico de sessões.
    fn log(&mut self, incoming: bool, kind: Kind, name: &str, id: String, control: bool, seconds: u64) {
        self.log_how(incoming, kind, name, id, control, seconds, false);
    }

    #[allow(clippy::too_many_arguments)]
    fn log_how(
        &mut self,
        incoming: bool,
        kind: Kind,
        name: &str,
        id: String,
        control: bool,
        seconds: u64,
        password: bool,
    ) {
        let when = opendesk_platform::system::local_time();
        let file = String::new();
        let entry = Entry { when, incoming, kind, name: name.to_string(), id, control, seconds, password, file };
        self.record(entry);
    }

    fn record(&mut self, entry: Entry) {
        self.history.append(&entry);
        self.history_view.insert(0, entry);
        self.history_view.truncate(200);
    }

    fn identity(&self) -> &Identity {
        self.node.identity()
    }

    /// Só em builds de desenvolvimento: `OPENDESK_PREVIEW=pedido,acesso,aviso,detalhes`
    /// abre o app já com esses estados na tela, para conferir o visual sem
    /// precisar de um segundo computador. O pedido falso não leva a lugar nenhum.
    #[cfg(debug_assertions)]
    fn preview(&mut self) {
        let Ok(states) = std::env::var("OPENDESK_PREVIEW") else { return };
        let fingerprint = self.identity().fingerprint();
        let peer = || PeerInfo {
            name: "NOTEBOOK-ANA".into(),
            fingerprint,
            addr: SocketAddr::from(([203, 0, 113, 24], 47700)),
            relay: None,
            wants: Permissions::FULL,
        };
        for state in states.split(',') {
            match state.trim() {
                "pedido" => {
                    let (reply, _) = oneshot::channel();
                    self.requests.push(Request { session: 0, peer: peer(), reply, since: Instant::now() });
                }
                "acesso" => self.inbound.push(Inbound {
                    session: 0,
                    peer: peer(),
                    permissions: Permissions::FULL,
                    since: Instant::now(),
                    by_password: false,
                }),
                "aviso" => {
                    self.notice =
                        Some("O computador 602 758 973 não está online. Confira o ID e tente de novo.".into());
                }
                "detalhes" => self.preview_details.set(true),
                "recentes" => {
                    let recent = |target: &str, name: &str, last: &str| crate::config::Recent {
                        target: target.into(),
                        name: name.into(),
                        last: last.into(),
                    };
                    self.recents = Recents::in_memory(vec![
                        recent("602 758 973", "NOTEBOOK-ANA", "2026-11-12 21:07"),
                        recent("170 292 961", "RECEPCAO-01", "2026-11-11 09:32"),
                        recent("192.168.0.42", "SERVIDOR-ARQUIVOS", "2026-11-08 17:15"),
                    ]);
                }
                "historico" => {
                    let entry = |incoming, kind, name: &str, control, seconds| Entry {
                        when: "2026-11-12 21:07".into(),
                        incoming,
                        kind,
                        name: name.into(),
                        id: "602 758 973".into(),
                        control,
                        seconds,
                        password: false,
                        file: String::new(),
                    };
                    self.history_view = vec![
                        entry(false, Kind::Session, "NOTEBOOK-ANA", true, 1_260),
                        entry(true, Kind::Session, "SUPORTE-TI", false, 340),
                        entry(true, Kind::Refused, "DESCONHECIDO", false, 0),
                        entry(true, Kind::Blocked, "DESCONHECIDO", false, 0),
                    ];
                }
                "arquivos" => {
                    let transfer = |session, incoming, name: &str, state| Transfer {
                        session,
                        peer: "NOTEBOOK-ANA".into(),
                        event: FileEvent { id: 0, incoming, name: name.into(), size: 4_800_000, state },
                        updated: Instant::now() + Duration::from_secs(3600),
                    };
                    let saved = FileState::Done(self.files_dir.join("contrato.pdf"));
                    self.transfers = vec![
                        transfer(Some(0), true, "contrato.pdf", saved.clone()),
                        transfer(Some(0), false, "planilha de custos.xlsx", FileState::Progress(2_100_000)),
                        transfer(None, false, "relatório.pdf", FileState::Progress(3_300_000)),
                        transfer(None, true, "fotos.zip", FileState::Offered),
                        transfer(None, true, "contrato.pdf", saved),
                    ];
                }
                "config" => self.screen = Screen::Settings(Tab::General),
                "config-seguranca" => self.screen = Screen::Settings(Tab::Security),
                "config-rede" => self.screen = Screen::Settings(Tab::Network),
                "config-historico" => self.screen = Screen::Settings(Tab::History),
                "config-sobre" => self.screen = Screen::Settings(Tab::About),
                _ => {}
            }
        }
    }

    /// Interpreta o que foi digitado: um ID de 9 dígitos ou um endereço.
    fn parse_target(&self, text: &str) -> Result<(Target, String), String> {
        if text.is_empty() {
            return Err("Informe o ID ou o endereço do computador remoto.".into());
        }
        if let Some(id) = DeviceId::parse(text) {
            return match &self.lookup {
                Some(lookup) => Ok((Target::Id { id, lookup: lookup.clone() }, id.to_string())),
                None => Err("Conectar por ID precisa de um servidor de encontro, e nenhum está configurado. \
                             Use o endereço do computador."
                    .into()),
            };
        }
        resolve(text).map(|addr| (Target::Address(addr), text.to_string()))
    }

    fn connect(&mut self) {
        let (target, label) = match self.parse_target(self.address.trim()) {
            Ok(parsed) => parsed,
            Err(message) => {
                self.notice = Some(message);
                return;
            }
        };
        let by_id = matches!(target, Target::Id { .. });
        let wants = if self.want_control { Permissions::FULL } else { Permissions::VIEW_ONLY };
        let (viewer, events) = Viewer::connect(
            self.runtime.handle(),
            self.node.clone(),
            target,
            self.device_name.clone(),
            wants,
            Some(self.files_dir.clone()),
            self.wake.clone(),
        );
        self.notice = None;
        self.outgoing = Some(Outgoing {
            viewer,
            events,
            target: label,
            by_id,
            host_name: String::new(),
            fingerprint: None,
            route: None,
            stage: Stage::Connecting,
            live_since: None,
            accepts_password: false,
            password: String::new(),
            password_try: PasswordTry::Idle,
            by_password: false,
            display_index: 0,
            displays: 1,
            paused: false,
            clipboard: None,
            texture: None,
            epoch: 0,
            size: [0, 0],
            buttons: HashSet::new(),
            stats: Stats { at: Instant::now(), frames: 0, bytes: 0, fps: 0.0, mbps: 0.0 },
        });
    }

    fn end_outgoing(&mut self, notice: Option<String>) {
        *self.key_target.lock().unwrap() = None;
        if let Some(grab) = &self.keyboard {
            grab.set_active(false);
        }
        if let Some(out) = self.outgoing.take()
            && let (Stage::Live { permissions }, Some(since)) = (&out.stage, out.live_since)
        {
            let id = out.fingerprint.map_or_else(|| out.target.clone(), |f| f.device_id().to_string());
            let seconds = since.elapsed().as_secs();
            self.log_how(false, Kind::Session, &out.host_name, id, permissions.control, seconds, out.by_password);
        }
        if std::mem::take(&mut self.fullscreen) {
            self.ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
        }
        self.notice = notice;
        // Fim da sessão: a janela volta ao tamanho compacto da tela inicial.
        if std::mem::take(&mut self.maximized_for_session) {
            self.ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(false));
        }
    }

    fn pump(&mut self, ctx: &egui::Context) {
        while let Ok(event) = self.host_events.try_recv() {
            match event {
                HostEvent::Request { session, peer, reply } => {
                    if !self.settings.accept_incoming {
                        // Entrada desligada: recusa sem incomodar e deixa registrado.
                        let id = peer.fingerprint.device_id().to_string();
                        self.log(true, Kind::Blocked, &peer.name, id, false, 0);
                        let _ = reply.send(None);
                        continue;
                    }
                    self.requests.push(Request { session, peer, reply, since: Instant::now() });
                    // Um pedido de acesso não pode passar despercebido.
                    if !self.quiet {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                    }
                }
                HostEvent::Started { session, peer, permissions, by_password } => {
                    // Se entrou pela senha, o pedido que estava na tela some.
                    self.requests.retain(|r| r.session != session);
                    self.inbound.push(Inbound { session, peer, permissions, since: Instant::now(), by_password });
                }
                HostEvent::File { session, transfer } => {
                    let peer = self.inbound.iter().find(|s| s.session == session).map(|s| s.peer.clone());
                    if let Some(peer) = peer {
                        let id = peer.fingerprint.device_id().to_string();
                        self.track(Some(session), &peer.name, id, transfer);
                    }
                }
                HostEvent::PasswordFailed { peer, .. } => {
                    let id = peer.fingerprint.device_id().to_string();
                    self.log(true, Kind::BadPassword, &peer.name, id, false, 0);
                }
                HostEvent::Ended { session, .. } => {
                    self.requests.retain(|r| r.session != session);
                    if let Some(index) = self.inbound.iter().position(|s| s.session == session) {
                        let ended = self.inbound.remove(index);
                        let id = ended.peer.fingerprint.device_id().to_string();
                        let seconds = ended.since.elapsed().as_secs();
                        let (control, password) = (ended.permissions.control, ended.by_password);
                        self.log_how(true, Kind::Session, &ended.peer.name, id, control, seconds, password);
                        if self.inbound.is_empty() && self.settings.lock_on_end && !self.quiet {
                            opendesk_platform::system::lock_workstation();
                        }
                    }
                }
            }
        }

        let mut closed = None;
        let mut granted = false;
        let mut files = Vec::new();
        if let Some(out) = &mut self.outgoing {
            while let Ok(event) = out.events.try_recv() {
                match event {
                    ViewerEvent::Connected { host_name, fingerprint, route, password } => {
                        out.accepts_password = password;
                        out.host_name = host_name;
                        out.fingerprint = Some(fingerprint);
                        out.route = Some(route);
                        out.stage = Stage::Waiting;
                    }
                    ViewerEvent::Granted { permissions, index, displays, .. } => {
                        out.stage = Stage::Live { permissions };
                        out.live_since = Some(Instant::now());
                        // A senha foi aceita se era ela que estava em conferência.
                        out.by_password = out.password_try == PasswordTry::Checking;
                        (out.display_index, out.displays) = (index, displays);
                        out.clipboard = permissions.clipboard.then(SystemClipboard::new);
                        self.recents.touch(&out.target, &out.host_name, opendesk_platform::system::local_time());
                        granted = true;
                        if permissions.control {
                            *self.key_target.lock().unwrap() = Some(out.viewer.input_sender());
                        }
                    }
                    ViewerEvent::PasswordRejected { retry } => out.password_try = PasswordTry::Wrong { retry },
                    ViewerEvent::Clipboard(text) => {
                        if let Some(clipboard) = &mut out.clipboard {
                            clipboard.set(&text);
                        }
                    }
                    ViewerEvent::Display { index, .. } => out.display_index = index,
                    ViewerEvent::File(transfer) => {
                        let id = out.fingerprint.map_or_else(|| out.target.clone(), |f| f.device_id().to_string());
                        files.push((out.host_name.clone(), id, transfer));
                    }
                    ViewerEvent::Denied(_) => {}
                    ViewerEvent::Closed(reason) => closed = Some(reason),
                }
            }
        }
        for (peer, id, transfer) in files {
            self.track(None, &peer, id, transfer);
        }
        self.pump_files(ctx);
        // O que foi copiado aqui vai para o computador remoto.
        if let Some(out) = &mut self.outgoing
            && let Some(text) = out.clipboard.as_mut().and_then(|clipboard| clipboard.poll())
        {
            out.viewer.send_clipboard(text);
        }
        // A tela remota precisa de espaço: a sessão abre com a janela maximizada.
        let large = ctx.input(|i| i.viewport().maximized == Some(true) || i.viewport().fullscreen == Some(true));
        if granted && !large && !self.quiet {
            ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(true));
            self.maximized_for_session = true;
        }
        if let Some(reason) = closed {
            self.end_outgoing(Some(reason));
        }

        // O teclado só é desviado para o remoto com a sessão em foco e com
        // permissão de controle.
        if let Some(grab) = &self.keyboard {
            let controlling = matches!(&self.outgoing, Some(Outgoing { stage: Stage::Live { permissions }, paused: false, .. }) if permissions.control);
            grab.set_active(controlling && ctx.input(|i| i.focused));
        }

        if !self.requests.is_empty() || !self.inbound.is_empty() || self.outgoing.is_some() || self.copied_at.is_some()
        {
            ctx.request_repaint_after(Duration::from_millis(500));
        }
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.pump(ctx);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let p = Palette::of(ui.ctx());
        let live = matches!(&self.outgoing, Some(Outgoing { stage: Stage::Live { .. }, .. }));
        if live {
            self.session_bar(ui, &p);
            self.transfer_strip(ui, &p);
            self.inbound_banner(ui, &p);
            self.remote_screen(ui, &p);
        } else {
            self.inbound_banner(ui, &p);
            self.home(ui, &p);
        }
        self.request_dialog(ui.ctx(), &p);
    }

    fn on_exit(&mut self) {
        self.outgoing = None;
        self.host.disconnect_all();
        self.node.close();
    }
}

// ───────────────────────────── Tela inicial ─────────────────────────────

impl App {
    fn home(&mut self, ui: &mut egui::Ui, p: &Palette) {
        let margin = |x: i8| Margin { left: x, right: x, top: 28, bottom: 20 };
        egui::Panel::left("side")
            .exact_size(SIDE_WIDTH)
            .resizable(false)
            .show_separator_line(false)
            .frame(egui::Frame::new().fill(p.side).inner_margin(margin(28)))
            .show(ui, |ui| self.this_computer(ui, p));
        egui::CentralPanel::default().frame(egui::Frame::new().fill(p.bg).inner_margin(margin(40))).show(ui, |ui| {
            match self.screen {
                Screen::Home => self.connect_form(ui, p),
                Screen::Settings(tab) => self.settings_screen(ui, p, tab),
            }
        });
    }

    fn this_computer(&mut self, ui: &mut egui::Ui, p: &Palette) {
        self.logo.show(ui, p, LOGO_HEIGHT);
        ui.add_space(LOGO_GAP);
        ui.spacing_mut().item_spacing.y = 6.0;

        // Com servidor de encontro, o que se compartilha é o ID; sem ele, o
        // endereço na rede.
        let by_id = self.lookup.is_some();
        let presence = self.presence.lock().unwrap().clone();
        let online = match &presence {
            Presence::Online { id, nat } if by_id => Some((*id, *nat)),
            _ => None,
        };
        let (title, value, copy, what) = match (by_id, online) {
            (true, Some((id, _))) => ("ID deste computador", Some(id.to_string()), "Copiar ID", "este ID"),
            (true, None) => ("ID deste computador", None, "Copiar ID", "este ID"),
            (false, _) => {
                ("Endereço deste computador", Some(self.local_address.clone()), "Copiar endereço", "este endereço")
            }
        };
        theme::section(ui, p, title);
        let font = theme::semibold(if by_id { 28.0 } else { 17.0 });
        let color = if value.is_some() { p.text } else { p.muted };
        let shown = value.as_deref().unwrap_or("··· ··· ···");
        ui.add(egui::Label::new(RichText::new(shown).font(font).color(color)).selectable(value.is_some()));

        let just_copied = self.copied_at.is_some_and(|at| at.elapsed() < Duration::from_secs(2));
        if !just_copied {
            self.copied_at = None;
        }
        let button = egui::Button::new(if just_copied { "Copiado" } else { copy });
        if ui.add_enabled(value.is_some(), button).clicked()
            && let Some(value) = value
        {
            ui.ctx().copy_text(value);
            self.copied_at = Some(Instant::now());
        }
        ui.add_space(4.0);
        let hint = if self.unattended_active() {
            format!("Passe {what} para quem vai acessar. Quem tiver a senha de acesso entra sem a sua aprovação.")
        } else if self.settings.accept_incoming {
            format!("Passe {what} para quem vai acessar. Você aprova cada conexão.")
        } else {
            "Este computador não está aceitando pedidos de acesso. Isso muda em Configurações, Segurança.".to_string()
        };
        ui.label(theme::muted(hint, p).small());

        ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
            self.status_row(ui, p, by_id, &presence, online.map(|(_, nat)| nat));
        });
    }

    /// Rodapé da coluna: o estado da presença e, sob demanda, os detalhes de
    /// identidade que a maioria das pessoas nunca precisa ver.
    fn status_row(&self, ui: &mut egui::Ui, p: &Palette, by_id: bool, presence: &Presence, nat: Option<Nat>) {
        ui.horizontal(|ui| {
            let (color, status, hint) = match (by_id, presence) {
                _ if !self.settings.accept_incoming => {
                    (p.muted, "Entrada desligada", Some("Todo pedido de acesso é recusado automaticamente."))
                }
                (false, _) => (
                    p.muted,
                    "Só por endereço",
                    Some("Nenhum servidor de encontro configurado. Este computador é acessível pelo endereço, na mesma rede."),
                ),
                (true, Presence::Online { .. }) => (p.ok, "Online", None),
                (true, Presence::Connecting) => (p.muted, "Conectando…", None),
                (true, Presence::Offline(reason)) => (p.danger, "Reconectando…", Some(reason.as_str())),
            };
            theme::dot(ui, color);
            let label = ui.label(theme::muted(status, p).small());
            if let Some(hint) = hint {
                label.on_hover_text(hint);
            }

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.visuals_mut().hyperlink_color = p.muted;
                let details = ui.link(RichText::new("Detalhes").small());
                #[cfg(debug_assertions)]
                if self.preview_details.take() {
                    egui::Popup::open_id(ui.ctx(), egui::Popup::default_response_id(&details));
                }
                egui::Popup::from_toggle_button_response(&details)
                    .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
                    .show(|ui| {
                        let fingerprint = self.identity().fingerprint();
                        egui::Grid::new("identity").num_columns(2).spacing([16.0, 6.0]).min_row_height(18.0).show(ui, |ui| {
                            theme::detail(ui, p, "Nome", &self.device_name);
                            if by_id {
                                theme::detail(ui, p, "Endereço na rede", &self.local_address);
                            } else {
                                theme::detail(ui, p, "ID", &fingerprint.device_id().to_string());
                            }
                            theme::detail(ui, p, "Impressão digital", &fingerprint.short());
                        });
                        if nat == Some(Nat::Symmetric) {
                            ui.set_max_width(300.0);
                            ui.label(
                                theme::muted(
                                    "Sua rede troca a porta a cada destino (NAT simétrico). Conexões vindas da internet podem precisar passar por um relay.",
                                    p,
                                )
                                .small(),
                            );
                        }
                    });
            });
        });
    }

    fn connect_form(&mut self, ui: &mut egui::Ui, p: &Palette) {
        // Uma linha da altura da marca, com o acesso às configurações; assim
        // os rótulos das duas colunas começam na mesma altura.
        let header = Vec2::new(ui.available_width().min(CONTENT_WIDTH), LOGO_HEIGHT);
        ui.allocate_ui_with_layout(header, Layout::right_to_left(Align::Center), |ui| {
            if ui.button("Configurações").clicked() {
                self.screen = Screen::Settings(Tab::General);
            }
        });
        ui.add_space(LOGO_GAP);
        ui.spacing_mut().item_spacing.y = 6.0;

        let busy = self.outgoing.is_some();
        let by_id = self.lookup.is_some();
        let label = theme::section(ui, p, "Acessar outro computador");

        let (mut connect, mut cancel) = (false, false);
        ui.horizontal(|ui| {
            ui.spacing_mut().interact_size.y = 36.0;
            let width = ui.available_width().min(CONTENT_WIDTH) - ACTION_WIDTH - ui.spacing().item_spacing.x;
            let field = ui
                .add_enabled(
                    !busy,
                    egui::TextEdit::singleline(&mut self.address)
                        .hint_text(if by_id { "ID ou endereço" } else { "Endereço, como 192.168.0.10" })
                        .font(egui::FontId::proportional(15.0))
                        .margin(Margin::symmetric(10, 8))
                        .desired_width(width),
                )
                .labelled_by(label.id);
            let submitted = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if busy {
                cancel = ui.add(egui::Button::new("Cancelar").min_size(Vec2::new(ACTION_WIDTH, 36.0))).clicked();
            } else {
                let ready = !self.address.trim().is_empty();
                connect =
                    theme::primary_button(ui, p, "Conectar", ready, ACTION_WIDTH).clicked() || (submitted && ready);
            }
        });
        ui.add_space(2.0);
        ui.add_enabled(!busy, egui::Checkbox::new(&mut self.want_control, "Controlar mouse e teclado"));

        if let Some(out) = &mut self.outgoing {
            let status = match out.stage {
                Stage::Connecting if out.by_id => format!("Procurando {}…", out.target),
                Stage::Connecting => format!("Conectando a {}…", out.target),
                _ => format!("Aguardando {} autorizar o acesso…", out.host_name),
            };
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().size(14.0).color(p.accent_text));
                ui.label(RichText::new(status).color(p.text));
            });
            // Quem está do outro lado: dá para conferir antes de a sessão abrir.
            if let Some(fingerprint) = out.fingerprint {
                ui.label(
                    theme::muted(
                        format!("ID {}, impressão digital {}", fingerprint.device_id(), fingerprint.short()),
                        p,
                    )
                    .small(),
                );
            }
            if out.accepts_password && matches!(out.stage, Stage::Waiting) {
                out.password_form(ui, p);
            }
        }
        if let Some(notice) = &self.notice {
            ui.add_space(6.0);
            egui::Frame::new()
                .fill(p.control)
                .corner_radius(CornerRadius::same(6))
                .inner_margin(Margin::symmetric(10, 8))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width().min(CONTENT_WIDTH) - 20.0);
                    ui.label(RichText::new(notice).color(p.text));
                });
        }
        let reopen = if busy { None } else { self.recent_list(ui, p) };

        ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
            let footer = Vec2::new(ui.available_width().min(CONTENT_WIDTH), 18.0);
            ui.allocate_ui_with_layout(footer, Layout::left_to_right(Align::Center), |ui| {
                let promise = if by_id {
                    "Conexão direta, cifrada de ponta a ponta."
                } else {
                    "Sem servidor de encontro: conexão por endereço, na mesma rede."
                };
                ui.label(theme::muted(promise, p).small());
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(theme::muted(concat!("v", env!("CARGO_PKG_VERSION")), p).small());
                });
            });
        });

        if cancel {
            self.end_outgoing(None);
        } else if let Some(target) = reopen {
            self.address = target;
            self.connect();
        } else if connect {
            self.connect();
        }
    }

    /// Os computadores acessados por último. Devolve o alvo escolhido, se a
    /// pessoa clicou em algum.
    fn recent_list(&mut self, ui: &mut egui::Ui, p: &Palette) -> Option<String> {
        if self.recents.items.is_empty() {
            return None;
        }
        ui.add_space(22.0);
        theme::section(ui, p, "Recentes");
        let width = ui.available_width().min(CONTENT_WIDTH);
        let (mut open, mut remove) = (None, None);
        // O rodapé da coluna precisa de espaço: a lista rola se não couber.
        egui::ScrollArea::vertical().max_height((ui.available_height() - 34.0).max(40.0)).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            for (index, item) in self.recents.items.iter().enumerate() {
                ui.horizontal(|ui| {
                    let when = crate::config::short_when(&item.last);
                    let link = Vec2::new(64.0, 34.0);
                    let row = width - link.x - ui.spacing().item_spacing.x;
                    if theme::list_row(ui, p, row, &item.name, &item.target, &when).clicked() {
                        open = Some(item.target.clone());
                    }
                    // Alinhado à borda direita do formulário.
                    ui.allocate_ui_with_layout(link, Layout::right_to_left(Align::Center), |ui| {
                        ui.visuals_mut().hyperlink_color = p.muted;
                        if ui.link(RichText::new("Remover").small()).clicked() {
                            remove = Some(index);
                        }
                    });
                });
            }
        });
        if let Some(index) = remove {
            self.recents.remove(index);
        }
        open
    }
}

// ─────────────────── Sessões de entrada: pedido e faixa ───────────────────

impl App {
    /// Faixa sempre visível enquanto alguém acessa este computador.
    fn inbound_banner(&mut self, ui: &mut egui::Ui, p: &Palette) {
        if self.inbound.is_empty() {
            return;
        }
        let (mut end, mut send) = (None, None);
        let sessions: Vec<u64> = self.inbound.iter().map(|s| s.session).collect();
        egui::Panel::top("inbound")
            .frame(egui::Frame::new().fill(p.alert_bg).inner_margin(Margin::symmetric(16, 6)))
            .show_separator_line(false)
            .show(ui, |ui| {
                for id in sessions {
                    let Some(session) = self.inbound.iter().find(|s| s.session == id) else { continue };
                    ui.horizontal(|ui| {
                        theme::dot(ui, p.alert_text);
                        let verb = if session.permissions.control { "está controlando" } else { "está vendo" };
                        let how = if session.by_password { ", pela senha de acesso" } else { "" };
                        let secs = session.since.elapsed().as_secs();
                        ui.label(
                            RichText::new(format!("{} {verb} este computador{how}", session.peer.name))
                                .font(theme::semibold(13.5))
                                .color(p.alert_text),
                        );
                        ui.label(RichText::new(format!("{:02}:{:02}", secs / 60, secs % 60)).color(p.alert_text));
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui.button("Encerrar acesso").clicked() {
                                end = Some(id);
                            }
                            if session.permissions.files && ui.button("Enviar arquivo").clicked() {
                                send = Some(id);
                            }
                        });
                    });
                    self.transfer_rows(ui, Some(id), p.alert_text);
                }
            });
        if let Some(session) = end {
            self.host.disconnect(session);
        }
        if let Some(session) = send {
            self.pick_files(Some(session));
        }
    }

    /// Arquivos da sessão em que este computador acessa outro: aparece só
    /// enquanto há algo acontecendo.
    fn transfer_strip(&mut self, ui: &mut egui::Ui, p: &Palette) {
        if !self.transfers.iter().any(|t| t.session.is_none()) {
            return;
        }
        egui::Panel::top("transfers")
            .frame(egui::Frame::new().fill(p.surface).inner_margin(Margin::symmetric(14, 5)))
            .show_separator_line(false)
            .show(ui, |ui| self.transfer_rows(ui, None, p.text));
    }

    fn request_dialog(&mut self, ctx: &egui::Context, p: &Palette) {
        let Some(request) = self.requests.first() else { return };
        let peer = &request.peer;
        let remaining = REQUEST_TTL.saturating_sub(request.since.elapsed()).as_secs();
        let mut answer: Option<Option<Permissions>> = None;
        let full = self.most_permissions();
        // Com o controle desligado nas configurações, só dá para deixar ver.
        let may_control = peer.wants.control && self.settings.allow_control;

        let frame = egui::Frame::new()
            .fill(p.surface)
            .stroke(Stroke::new(1.0, p.border))
            .corner_radius(CornerRadius::same(10))
            .inner_margin(Margin::same(20))
            .shadow(ctx.style_of(ctx.theme()).visuals.window_shadow);
        egui::Modal::new(egui::Id::unique("access-request")).frame(frame).show(ctx, |ui| {
            ui.set_width(400.0);
            let wants = if peer.wants.control { "ver e controlar" } else { "ver a tela de" };
            ui.label(
                RichText::new(format!("{} quer {wants} este computador", peer.name))
                    .font(theme::semibold(16.0))
                    .color(p.text),
            );
            ui.add_space(2.0);
            egui::Grid::new("request").num_columns(2).spacing([16.0, 6.0]).min_row_height(18.0).show(ui, |ui| {
                theme::detail(ui, p, "ID", &peer.fingerprint.device_id().to_string());
                theme::detail(ui, p, "Impressão digital", &peer.fingerprint.short());
                let network = match peer.relay {
                    Some(relay) => format!("via relay {relay}"),
                    None => peer.addr.to_string(),
                };
                theme::detail(ui, p, "Endereço de rede", &network);
            });
            ui.add_space(2.0);
            theme::alert(
                ui,
                p,
                "Só aceite se você conhece quem está pedindo e estava esperando por isso. Bancos, lojas e suporte técnico de verdade não pedem acesso ao seu computador por telefone.",
            );
            if peer.wants.control && !may_control {
                ui.label(
                    theme::muted("O controle de mouse e teclado está desligado nas configurações deste computador.", p)
                        .small(),
                );
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Recusar").clicked() {
                    answer = Some(None);
                }
                ui.label(theme::muted(format!("expira em {remaining} s"), p).small());
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if may_control {
                        if theme::primary_button(ui, p, "Permitir controle", true, 0.0).clicked() {
                            answer = Some(Some(full));
                        }
                        if ui.button("Somente ver").clicked() {
                            answer = Some(Some(Permissions::VIEW_ONLY));
                        }
                    } else if theme::primary_button(ui, p, "Permitir ver", true, 0.0).clicked() {
                        answer = Some(Some(Permissions::VIEW_ONLY));
                    }
                });
            });
        });

        if let Some(answer) = answer {
            let request = self.requests.remove(0);
            if answer.is_none() {
                let id = request.peer.fingerprint.device_id().to_string();
                self.log(true, Kind::Refused, &request.peer.name, id, false, 0);
            }
            let _ = request.reply.send(answer);
        }
    }
}

// ───────────────────────────── Sessão remota ─────────────────────────────

impl App {
    fn session_bar(&mut self, ui: &mut egui::Ui, p: &Palette) {
        let Some(out) = &mut self.outgoing else { return };
        let Stage::Live { permissions } = out.stage else { return };
        out.stats.sample(&out.viewer);

        // Em tela cheia a barra sai do caminho e volta quando o ponteiro
        // encosta no topo (ou enquanto um menu dela está aberto).
        if self.fullscreen {
            let at_top = ui.ctx().input(|i| i.pointer.hover_pos().is_some_and(|pos| pos.y < 6.0));
            if at_top || egui::Popup::is_any_open(ui.ctx()) {
                self.bar_until = Some(Instant::now() + Duration::from_millis(1500));
            }
            if self.bar_until.is_none_or(|until| Instant::now() >= until) {
                return;
            }
        }

        let (mut end, mut toggle_fullscreen, mut pick) = (false, false, false);
        let fullscreen = self.fullscreen;
        let bar = egui::Panel::top("session-bar")
            .frame(egui::Frame::new().fill(p.side).inner_margin(Margin::symmetric(14, 6)))
            .show_separator_line(false)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    theme::dot(ui, p.ok);
                    ui.label(RichText::new(&out.host_name).font(theme::semibold(13.5)).color(p.text));
                    // Transparência: por onde a sessão passa e com que atraso.
                    if let Some(route) = out.route {
                        let latency = match out.viewer.rtt() {
                            Some(rtt) if rtt < Duration::from_millis(1) => ", menos de 1 ms".to_string(),
                            Some(rtt) => format!(", {} ms", rtt.as_millis()),
                            None => String::new(),
                        };
                        let explanation = match route {
                            Route::Relay => {
                                "Não houve caminho direto, então os pacotes passam por um relay. Eles continuam cifrados de ponta a ponta: o relay não vê a tela."
                            }
                            _ => {
                                "A sessão vai direto de um computador ao outro, cifrada de ponta a ponta. Nenhum servidor vê a tela."
                            }
                        };
                        ui.label(theme::muted(format!("{}{latency}", route.label()), p).small())
                            .on_hover_text(explanation);
                    }
                    if let Some(since) = out.live_since {
                        let secs = since.elapsed().as_secs();
                        let clock = match secs / 3600 {
                            0 => format!("{:02}:{:02}", secs / 60, secs % 60),
                            hours => format!("{hours}:{:02}:{:02}", secs % 3600 / 60, secs % 60),
                        };
                        ui.label(theme::muted(clock, p).small()).on_hover_text("Duração da sessão");
                    }

                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.button("Encerrar").clicked() {
                            end = true;
                        }
                        if permissions.control {
                            ui.menu_button("Ações", |ui| {
                                if permissions.files && ui.button("Enviar arquivos…").clicked() {
                                    pick = true;
                                    ui.close();
                                }
                                let pause = if out.paused { "Retomar o controle" } else { "Pausar o controle" };
                                if ui.button(pause).clicked() {
                                    out.paused = !out.paused;
                                    ui.close();
                                }
                                if ui.button("Bloquear o computador remoto").clicked() {
                                    out.viewer.lock_remote();
                                    ui.close();
                                }
                            });
                        }
                        let label = if fullscreen { "Sair da tela cheia" } else { "Tela cheia" };
                        toggle_fullscreen = ui.button(label).clicked();
                        if out.displays > 1 {
                            let label = format!("Monitor {} de {}", out.display_index + 1, out.displays);
                            if ui.button(label).on_hover_text("Mostrar o próximo monitor").clicked() {
                                out.viewer.select_display((out.display_index + 1) % out.displays);
                            }
                        }
                        let mode = match (permissions.control, out.paused) {
                            (true, false) => "Controle ativo",
                            (true, true) => "Controle pausado",
                            (false, _) => "Somente visualização",
                        };
                        ui.label(theme::muted(mode, p).small());
                        ui.add_space(8.0);
                        // Transparência sem ruído: o essencial na barra, o resto no ponteiro.
                        ui.label(
                            theme::muted(format!("{:.0} quadros/s   {:.1} Mbit/s", out.stats.fps, out.stats.mbps), p)
                                .small(),
                        )
                        .on_hover_text(format!("Tela de {}×{}, codec Tiles v1", out.size[0], out.size[1]));
                    });
                });
            });
        // Com o ponteiro na barra, ela fica.
        if self.fullscreen && bar.response.contains_pointer() {
            self.bar_until = Some(Instant::now() + Duration::from_millis(1500));
        }
        if toggle_fullscreen {
            self.fullscreen = !self.fullscreen;
            self.bar_until = Some(Instant::now() + Duration::from_millis(1500));
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.fullscreen));
        }
        if pick {
            self.pick_files(None);
        }
        if end {
            self.end_outgoing(None);
        }
    }

    fn remote_screen(&mut self, ui: &mut egui::Ui, p: &Palette) {
        let Some(out) = &mut self.outgoing else { return };
        let Stage::Live { permissions } = out.stage else { return };
        egui::CentralPanel::default().frame(egui::Frame::new().fill(p.stage)).show(ui, |ui| {
            out.sync_texture(ui.ctx());
            let area = ui.available_rect_before_wrap();
            ui.allocate_rect(area, Sense::click_and_drag());
            let Some(texture) = &out.texture else {
                ui.put(Rect::from_center_size(area.center(), Vec2::splat(28.0)), egui::Spinner::new().size(28.0));
                return;
            };
            // Ajusta à janela preservando a proporção.
            let [width, height] = out.size;
            let scale = (area.width() / width as f32).min(area.height() / height as f32);
            let image = Rect::from_center_size(area.center(), Vec2::new(width as f32, height as f32) * scale);
            let uv = Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
            ui.painter().image(texture.id(), image, uv, Color32::WHITE);
            if permissions.control && !out.paused {
                out.forward_pointer(ui, image);
            }
        });
    }
}

impl Outgoing {
    /// Entrar com a senha de acesso não supervisionado, enquanto o pedido
    /// aguarda alguém aprovar do outro lado.
    fn password_form(&mut self, ui: &mut egui::Ui, p: &Palette) {
        ui.add_space(10.0);
        let label = ui.label(theme::muted("Ou entre com a senha de acesso desse computador:", p).small());
        let locked = self.password_try == PasswordTry::Wrong { retry: false };
        let checking = self.password_try == PasswordTry::Checking;
        ui.horizontal(|ui| {
            ui.spacing_mut().interact_size.y = 32.0;
            let field = ui
                .add_enabled(
                    !locked && !checking,
                    egui::TextEdit::singleline(&mut self.password)
                        .password(true)
                        .hint_text("Senha de acesso")
                        .margin(Margin::symmetric(10, 7))
                        .desired_width(220.0),
                )
                .labelled_by(label.id);
            let entered = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            let ready = !self.password.is_empty() && !locked && !checking;
            if (theme::primary_button(ui, p, "Entrar", ready, 84.0).clicked() || entered) && ready {
                self.viewer.submit_password(std::mem::take(&mut self.password));
                self.password_try = PasswordTry::Checking;
            }
        });
        match self.password_try {
            PasswordTry::Idle => {}
            PasswordTry::Checking => {
                ui.label(theme::muted("Conferindo a senha…", p).small());
            }
            PasswordTry::Wrong { retry: true } => {
                ui.label(RichText::new("Senha incorreta.").small().color(p.danger));
            }
            PasswordTry::Wrong { retry: false } => {
                ui.label(
                    RichText::new(
                        "Senha incorreta. Esse computador não aceita novas tentativas por enquanto; o pedido continua valendo se alguém aprovar lá.",
                    )
                    .small()
                    .color(p.danger),
                );
            }
        }
    }

    /// Copia para a textura só a região que mudou desde o último quadro
    /// desenhado.
    fn sync_texture(&mut self, ctx: &egui::Context) {
        let update = {
            let mut frame = self.viewer.frame.lock().unwrap();
            if frame.width == 0 {
                return;
            }
            let recreate = self.texture.is_none() || self.epoch != frame.epoch;
            let region = if recreate {
                frame.dirty = None;
                Some([0, 0, frame.width, frame.height])
            } else {
                frame.take_dirty()
            };
            self.epoch = frame.epoch;
            self.size = [frame.width, frame.height];
            region.map(|[x0, y0, x1, y1]| {
                let (w, h) = (x1 - x0, y1 - y0);
                let mut pixels = Vec::with_capacity(w * h * 4);
                for y in y0..y1 {
                    let start = (y * frame.width + x0) * 4;
                    pixels.extend_from_slice(&frame.rgba[start..start + w * 4]);
                }
                (recreate, [x0, y0], ColorImage::from_rgba_premultiplied([w, h], &pixels))
            })
        };
        match (update, &mut self.texture) {
            (Some((false, pos, image)), Some(texture)) => texture.set_partial(pos, image, TextureOptions::LINEAR),
            (Some((_, _, image)), _) => {
                self.texture = Some(ctx.load_texture("tela-remota", image, TextureOptions::LINEAR));
            }
            (None, _) => {}
        }
    }

    fn forward_pointer(&mut self, ui: &egui::Ui, image: Rect) {
        let normalize = |pos: Pos2| {
            let x = ((pos.x - image.left()) / image.width()).clamp(0.0, 1.0);
            let y = ((pos.y - image.top()) / image.height()).clamp(0.0, 1.0);
            ((x * 65535.0) as u16, (y * 65535.0) as u16)
        };
        let (events, hover) = ui.input(|i| (i.events.clone(), i.pointer.hover_pos()));
        let over_image = hover.is_some_and(|pos| image.contains(pos));
        for event in events {
            match event {
                // Durante um arrasto o movimento continua valendo fora da imagem.
                Event::PointerMoved(pos) if image.contains(pos) || !self.buttons.is_empty() => {
                    let (x, y) = normalize(pos);
                    self.viewer.send_input(InputEvent::MouseMove { x, y });
                }
                Event::PointerButton { pos, button, pressed, .. } => {
                    let button = match button {
                        PointerButton::Primary => MouseButton::Left,
                        PointerButton::Secondary => MouseButton::Right,
                        PointerButton::Middle => MouseButton::Middle,
                        PointerButton::Extra1 => MouseButton::Back,
                        PointerButton::Extra2 => MouseButton::Forward,
                    };
                    // Só começa clique dentro da imagem; soltar vale em
                    // qualquer lugar, para não deixar botão preso no remoto.
                    let relevant = if pressed {
                        image.contains(pos) && self.buttons.insert(button)
                    } else {
                        self.buttons.remove(&button)
                    };
                    if relevant {
                        let (x, y) = normalize(pos);
                        self.viewer.send_input(InputEvent::MouseButton { button, down: pressed, x, y });
                    }
                }
                Event::MouseWheel { unit, delta, .. } if over_image => {
                    let notch = match unit {
                        MouseWheelUnit::Line => 120.0,
                        MouseWheelUnit::Point => 2.4,
                        MouseWheelUnit::Page => 360.0,
                    };
                    let (dx, dy) = ((delta.x * notch) as i16, (delta.y * notch) as i16);
                    if dx != 0 || dy != 0 {
                        self.viewer.send_input(InputEvent::Wheel { dx, dy });
                    }
                }
                _ => {}
            }
        }
    }
}

impl Stats {
    fn sample(&mut self, viewer: &Viewer) {
        let elapsed = self.at.elapsed().as_secs_f32();
        if elapsed < 1.0 {
            return;
        }
        let (frames, bytes) = {
            let frame = viewer.frame.lock().unwrap();
            (frame.frames, frame.bytes)
        };
        self.fps = (frames - self.frames) as f32 / elapsed;
        self.mbps = (bytes - self.bytes) as f32 * 8.0 / 1e6 / elapsed;
        self.frames = frames;
        self.bytes = bytes;
        self.at = Instant::now();
    }
}

// ───────────────────────────── Utilidades ─────────────────────────────

/// Aceita `ip`, `ip:porta`, `nome` ou `nome:porta`.
fn resolve(target: &str) -> Result<SocketAddr, String> {
    let port = opendesk_proto::DEFAULT_PORT;
    if target.is_empty() {
        return Err("Informe o endereço do computador remoto.".into());
    }
    if let Ok(addr) = target.parse::<SocketAddr>() {
        return Ok(addr);
    }
    if let Ok(ip) = target.parse::<IpAddr>() {
        return Ok(SocketAddr::new(ip, port));
    }
    let with_port = if target.contains(':') { target.to_string() } else { format!("{target}:{port}") };
    with_port
        .to_socket_addrs()
        .ok()
        .and_then(|mut addrs| addrs.find(SocketAddr::is_ipv4))
        .ok_or_else(|| format!("Não foi possível encontrar \"{target}\". Confira o endereço."))
}
