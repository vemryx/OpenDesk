//! Estado e telas do aplicativo.
//!
//! Uma janela só, compacta: a tela inicial (o ID deste computador ao lado do
//! campo para acessar outro) dá lugar à visão remota, em janela maximizada,
//! quando uma sessão de saída é autorizada. Pedidos de entrada aparecem como
//! diálogo modal por cima de qualquer tela, e uma sessão de entrada ativa fica
//! sempre anunciada numa faixa no topo.

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
use opendesk_session::backend::SystemBackend;
use opendesk_session::host::{Host, HostEvent, PeerInfo};
use opendesk_session::viewer::{InputSender, Lookup, Route, Target, Viewer, ViewerEvent};
use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};

/// O host desiste do pedido depois deste tempo (ver `opendesk-session`).
const REQUEST_TTL: Duration = Duration::from_secs(60);

/// Tamanho da janela na tela inicial.
pub const HOME_SIZE: [f32; 2] = [720.0, 360.0];
/// Largura do conteúdo da coluna "este computador".
const SIDE_WIDTH: f32 = 232.0;
/// Altura do símbolo da marca e o respiro abaixo dele. A coluna da direita
/// desce o mesmo tanto, para os rótulos das duas colunas ficarem alinhados.
const LOGO_HEIGHT: f32 = 26.0;
const LOGO_GAP: f32 = 26.0;
/// Largura do botão ao lado do campo (igual para "Conectar" e "Cancelar",
/// para o campo não mudar de tamanho entre os estados).
const ACTION_WIDTH: f32 = 104.0;

pub struct App {
    runtime: tokio::runtime::Runtime,
    node: Arc<Node>,
    host: Arc<Host>,
    host_events: mpsc::UnboundedReceiver<HostEvent>,
    wake: Wake,
    ctx: egui::Context,
    logo: theme::Logo,
    device_name: String,
    local_address: String,
    /// Busca por ID; `None` quando não há servidor de encontro configurado.
    lookup: Option<Arc<Lookup>>,
    presence: Arc<Mutex<Presence>>,

    requests: Vec<Request>,
    inbound: Vec<Inbound>,

    address: String,
    want_control: bool,
    outgoing: Option<Outgoing>,
    notice: Option<String>,
    copied_at: Option<Instant>,
    /// A janela foi maximizada pelo app ao entrar na sessão (e não pela pessoa).
    maximized_for_session: bool,
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
        theme::install(&cc.egui_ctx);

        let data_dir = match std::env::var_os("OPENDESK_DATA_DIR") {
            Some(dir) => dir.into(),
            None => directories::ProjectDirs::from("org", "OpenDesk", "OpenDesk")
                .context("não foi possível localizar a pasta de dados do usuário")?
                .data_dir()
                .to_path_buf(),
        };
        let identity = Arc::new(Identity::load_or_create(&data_dir)?);
        let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().context("iniciar runtime")?;

        let port =
            std::env::var("OPENDESK_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(opendesk_proto::DEFAULT_PORT);
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
        let device_name = std::env::var("COMPUTERNAME")
            .or_else(|_| std::env::var("HOSTNAME"))
            .unwrap_or_else(|_| "Meu computador".into());
        let (host, host_events) =
            Host::new(node.clone(), Arc::new(SystemBackend { display: 0 }), device_name.clone(), wake.clone());
        runtime.spawn(host.clone().serve());

        // Com um servidor de encontro, este computador fica acessível pelo ID.
        let lookup = Lookup::from_env(&data_dir).unwrap_or_else(|err| {
            tracing::warn!("servidor de encontro mal configurado: {err:#}");
            None
        });
        let presence = Arc::new(Mutex::new(Presence::Connecting));
        if let Some(lookup) = lookup.clone() {
            let (node, presence, wake) = (node.clone(), presence.clone(), wake.clone());
            runtime.spawn(async move {
                let servers = lookup.stun_servers.clone();
                let update = move |now: Presence| {
                    *presence.lock().unwrap() = now;
                    wake();
                };
                lookup.rendezvous.serve(node, servers, lookup.relay.clone(), update).await;
            });
        }

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

        let mut app = Self {
            runtime,
            node,
            host,
            host_events,
            wake,
            ctx: cc.egui_ctx.clone(),
            logo: theme::Logo::load(),
            device_name,
            local_address,
            lookup,
            presence,
            requests: Vec::new(),
            inbound: Vec::new(),
            address: String::new(),
            want_control: true,
            outgoing: None,
            notice: None,
            copied_at: None,
            maximized_for_session: false,
            quiet: std::env::var_os("OPENDESK_WINDOW_POS").is_some(),
            #[cfg(debug_assertions)]
            preview_details: Default::default(),
            keyboard,
            key_target,
        };
        #[cfg(debug_assertions)]
        app.preview();
        // `opendesk <ID ou endereço>` conecta direto ao abrir.
        if let Some(target) = std::env::args().nth(1) {
            app.address = target;
            app.connect();
        }
        Ok(app)
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
                }),
                "aviso" => {
                    self.notice =
                        Some("O computador 602 758 973 não está online. Confira o ID e tente de novo.".into());
                }
                "detalhes" => self.preview_details.set(true),
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
        self.outgoing = None;
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
                    self.requests.push(Request { session, peer, reply, since: Instant::now() });
                    // Um pedido de acesso não pode passar despercebido.
                    if !self.quiet {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                    }
                }
                HostEvent::Started { session, peer, permissions } => {
                    self.inbound.push(Inbound { session, peer, permissions, since: Instant::now() });
                }
                HostEvent::Ended { session, .. } => {
                    self.requests.retain(|r| r.session != session);
                    self.inbound.retain(|s| s.session != session);
                }
            }
        }

        let mut closed = None;
        let mut granted = false;
        if let Some(out) = &mut self.outgoing {
            while let Ok(event) = out.events.try_recv() {
                match event {
                    ViewerEvent::Connected { host_name, fingerprint, route } => {
                        out.host_name = host_name;
                        out.fingerprint = Some(fingerprint);
                        out.route = Some(route);
                        out.stage = Stage::Waiting;
                    }
                    ViewerEvent::Granted { permissions, .. } => {
                        out.stage = Stage::Live { permissions };
                        granted = true;
                        if permissions.control {
                            *self.key_target.lock().unwrap() = Some(out.viewer.input_sender());
                        }
                    }
                    ViewerEvent::Denied(_) => {}
                    ViewerEvent::Closed(reason) => closed = Some(reason),
                }
            }
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
            let controlling = matches!(&self.outgoing, Some(Outgoing { stage: Stage::Live { permissions }, .. }) if permissions.control);
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
        let margin = |x: i8| Margin { left: x, right: x, top: 20, bottom: 14 };
        egui::Panel::left("side")
            .exact_size(SIDE_WIDTH)
            .resizable(false)
            .show_separator_line(false)
            .frame(egui::Frame::new().fill(p.side).inner_margin(margin(22)))
            .show(ui, |ui| self.this_computer(ui, p));
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(p.bg).inner_margin(margin(28)))
            .show(ui, |ui| self.connect_form(ui, p));
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
        ui.label(theme::muted(format!("Passe {what} para quem vai acessar. Você aprova cada conexão."), p).small());

        ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
            self.status_row(ui, p, by_id, &presence, online.map(|(_, nat)| nat));
        });
    }

    /// Rodapé da coluna: o estado da presença e, sob demanda, os detalhes de
    /// identidade que a maioria das pessoas nunca precisa ver.
    fn status_row(&self, ui: &mut egui::Ui, p: &Palette, by_id: bool, presence: &Presence, nat: Option<Nat>) {
        ui.horizontal(|ui| {
            let (color, status, hint) = match (by_id, presence) {
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
        // Mesma descida da coluna ao lado: marca, espaçamento entre itens e respiro.
        ui.add_space(LOGO_HEIGHT + ui.spacing().item_spacing.y + LOGO_GAP);
        ui.spacing_mut().item_spacing.y = 6.0;

        let busy = self.outgoing.is_some();
        let by_id = self.lookup.is_some();
        let label = theme::section(ui, p, "Acessar outro computador");

        let (mut connect, mut cancel) = (false, false);
        ui.horizontal(|ui| {
            ui.spacing_mut().interact_size.y = 36.0;
            let width = ui.available_width() - ACTION_WIDTH - ui.spacing().item_spacing.x;
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

        if let Some(out) = &self.outgoing {
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
        }
        if let Some(notice) = &self.notice {
            ui.add_space(6.0);
            egui::Frame::new()
                .fill(p.control)
                .corner_radius(CornerRadius::same(6))
                .inner_margin(Margin::symmetric(10, 8))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(RichText::new(notice).color(p.text));
                });
        }

        ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
            ui.horizontal(|ui| {
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
        } else if connect {
            self.connect();
        }
    }
}

// ─────────────────── Sessões de entrada: pedido e faixa ───────────────────

impl App {
    /// Faixa sempre visível enquanto alguém acessa este computador.
    fn inbound_banner(&mut self, ui: &mut egui::Ui, p: &Palette) {
        if self.inbound.is_empty() {
            return;
        }
        let mut end = None;
        egui::Panel::top("inbound")
            .frame(egui::Frame::new().fill(p.alert_bg).inner_margin(Margin::symmetric(16, 6)))
            .show_separator_line(false)
            .show(ui, |ui| {
                for session in &self.inbound {
                    ui.horizontal(|ui| {
                        theme::dot(ui, p.alert_text);
                        let verb = if session.permissions.control { "está controlando" } else { "está vendo" };
                        let secs = session.since.elapsed().as_secs();
                        ui.label(
                            RichText::new(format!("{} {verb} este computador", session.peer.name))
                                .font(theme::semibold(13.5))
                                .color(p.alert_text),
                        );
                        ui.label(RichText::new(format!("{:02}:{:02}", secs / 60, secs % 60)).color(p.alert_text));
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui.button("Encerrar acesso").clicked() {
                                end = Some(session.session);
                            }
                        });
                    });
                }
            });
        if let Some(session) = end {
            self.host.disconnect(session);
        }
    }

    fn request_dialog(&mut self, ctx: &egui::Context, p: &Palette) {
        let Some(request) = self.requests.first() else { return };
        let peer = &request.peer;
        let remaining = REQUEST_TTL.saturating_sub(request.since.elapsed()).as_secs();
        let mut answer: Option<Option<Permissions>> = None;

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
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Recusar").clicked() {
                    answer = Some(None);
                }
                ui.label(theme::muted(format!("expira em {remaining} s"), p).small());
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if peer.wants.control {
                        if theme::primary_button(ui, p, "Permitir controle", true, 0.0).clicked() {
                            answer = Some(Some(Permissions::FULL));
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
        let mut end = false;
        egui::Panel::top("session-bar")
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
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.button("Encerrar").clicked() {
                            end = true;
                        }
                        let mode = if permissions.control { "Controle ativo" } else { "Somente visualização" };
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
            if permissions.control {
                out.forward_pointer(ui, image);
            }
        });
    }
}

impl Outgoing {
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
