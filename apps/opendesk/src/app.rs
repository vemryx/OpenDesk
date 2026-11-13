//! Estado e telas do aplicativo.
//!
//! Uma janela só: a tela inicial (este computador + conectar a outro) dá
//! lugar à visão remota quando uma sessão de saída é autorizada. Pedidos de
//! entrada aparecem como diálogo modal por cima de qualquer tela, e uma sessão
//! de entrada ativa fica sempre anunciada numa faixa no topo.

use crate::theme::{self, Palette};
use anyhow::{Context as _, Result};
use eframe::egui::{
    self, Align, Color32, ColorImage, CornerRadius, Event, Layout, Margin, MouseWheelUnit, PointerButton, Pos2, Rect,
    RichText, Sense, Stroke, TextureHandle, TextureOptions, Vec2,
};
use opendesk_net::{Fingerprint, Identity, Node};
use opendesk_platform::keyboard::KeyboardGrab;
use opendesk_proto::control::{InputEvent, MouseButton, Permissions};
use opendesk_session::Wake;
use opendesk_session::backend::SystemBackend;
use opendesk_session::host::{Host, HostEvent, PeerInfo};
use opendesk_session::viewer::{InputSender, Viewer, ViewerEvent};
use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};

/// O host desiste do pedido depois deste tempo (ver `opendesk-session`).
const REQUEST_TTL: Duration = Duration::from_secs(60);

pub struct App {
    runtime: tokio::runtime::Runtime,
    node: Arc<Node>,
    host: Arc<Host>,
    host_events: mpsc::UnboundedReceiver<HostEvent>,
    wake: Wake,
    device_name: String,
    local_address: String,

    requests: Vec<Request>,
    inbound: Vec<Inbound>,

    address: String,
    want_control: bool,
    outgoing: Option<Outgoing>,
    notice: Option<String>,
    copied_at: Option<Instant>,

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
    host_name: String,
    fingerprint: Option<Fingerprint>,
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
        let local_address = match local_ip() {
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
            device_name,
            local_address,
            requests: Vec::new(),
            inbound: Vec::new(),
            address: String::new(),
            want_control: true,
            outgoing: None,
            notice: None,
            copied_at: None,
            keyboard,
            key_target,
        };
        // `opendesk <endereço>` conecta direto ao abrir.
        if let Some(target) = std::env::args().nth(1) {
            app.address = target;
            app.connect();
        }
        Ok(app)
    }

    fn identity(&self) -> &Identity {
        self.node.identity()
    }

    fn connect(&mut self) {
        let target = self.address.trim().to_string();
        match resolve(&target) {
            Ok(addr) => {
                let wants = if self.want_control { Permissions::FULL } else { Permissions::VIEW_ONLY };
                let (viewer, events) = Viewer::connect(
                    self.runtime.handle(),
                    self.node.clone(),
                    addr,
                    self.device_name.clone(),
                    wants,
                    self.wake.clone(),
                );
                self.notice = None;
                self.outgoing = Some(Outgoing {
                    viewer,
                    events,
                    target,
                    host_name: String::new(),
                    fingerprint: None,
                    stage: Stage::Connecting,
                    texture: None,
                    epoch: 0,
                    size: [0, 0],
                    buttons: HashSet::new(),
                    stats: Stats { at: Instant::now(), frames: 0, bytes: 0, fps: 0.0, mbps: 0.0 },
                });
            }
            Err(message) => self.notice = Some(message),
        }
    }

    fn end_outgoing(&mut self, notice: Option<String>) {
        *self.key_target.lock().unwrap() = None;
        if let Some(grab) = &self.keyboard {
            grab.set_active(false);
        }
        self.outgoing = None;
        self.notice = notice;
    }

    fn pump(&mut self, ctx: &egui::Context) {
        while let Ok(event) = self.host_events.try_recv() {
            match event {
                HostEvent::Request { session, peer, reply } => {
                    self.requests.push(Request { session, peer, reply, since: Instant::now() });
                    // Um pedido de acesso não pode passar despercebido.
                    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
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
        if let Some(out) = &mut self.outgoing {
            while let Ok(event) = out.events.try_recv() {
                match event {
                    ViewerEvent::Connected { host_name, fingerprint } => {
                        out.host_name = host_name;
                        out.fingerprint = Some(fingerprint);
                        out.stage = Stage::Waiting;
                    }
                    ViewerEvent::Granted { permissions, .. } => {
                        out.stage = Stage::Live { permissions };
                        if permissions.control {
                            *self.key_target.lock().unwrap() = Some(out.viewer.input_sender());
                        }
                    }
                    ViewerEvent::Denied(_) => {}
                    ViewerEvent::Closed(reason) => closed = Some(reason),
                }
            }
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
            self.remote_screen(ui);
        } else {
            self.header(ui, &p);
            self.inbound_banner(ui, &p);
            self.footer(ui, &p);
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
    fn header(&mut self, ui: &mut egui::Ui, p: &Palette) {
        egui::Panel::top("header")
            .frame(egui::Frame::new().fill(p.bg).inner_margin(Margin { left: 28, right: 28, top: 20, bottom: 8 }))
            .show_separator_line(false)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("OpenDesk").font(theme::semibold(20.0)).color(p.text));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(theme::muted("Pronto para receber conexões", p));
                        theme::dot(ui, p.accent);
                    });
                });
            });
    }

    fn footer(&mut self, ui: &mut egui::Ui, p: &Palette) {
        egui::Panel::bottom("footer")
            .frame(egui::Frame::new().fill(p.bg).inner_margin(Margin { left: 28, right: 28, top: 8, bottom: 16 }))
            .show_separator_line(false)
            .show(ui, |ui| {
                ui.label(
                    theme::muted(
                        format!(
                            "OpenDesk {} · código aberto (Apache-2.0) · conexões cifradas de ponta a ponta",
                            env!("CARGO_PKG_VERSION")
                        ),
                        p,
                    )
                    .small(),
                );
            });
    }

    fn home(&mut self, ui: &mut egui::Ui, p: &Palette) {
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(p.bg).inner_margin(Margin { left: 28, right: 28, top: 12, bottom: 8 }))
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.x = 20.0;
                let height = ui.available_height();
                ui.columns(2, |columns| {
                    for (column, right) in columns.iter_mut().zip([false, true]) {
                        theme::card(p).show(column, |ui| {
                            // As colunas justificam o conteúdo; dentro do
                            // cartão o alinhamento é o natural, à esquerda.
                            let size = Vec2::new(ui.available_width(), height - 50.0);
                            ui.allocate_ui_with_layout(size, Layout::top_down(Align::Min), |ui| {
                                ui.set_min_size(size);
                                if right { self.connect_form(ui, p) } else { self.this_computer(ui, p) }
                            });
                        });
                    }
                });
            });
    }

    fn this_computer(&mut self, ui: &mut egui::Ui, p: &Palette) {
        ui.spacing_mut().item_spacing.x = 10.0;
        theme::caption(ui, p, "ESTE COMPUTADOR");
        ui.add_space(6.0);
        ui.label(theme::muted("Endereço para conexão", p));
        ui.add(
            egui::Label::new(RichText::new(&self.local_address).font(theme::semibold(28.0)).color(p.text))
                .selectable(true),
        );
        ui.add_space(2.0);
        let just_copied = self.copied_at.is_some_and(|at| at.elapsed() < Duration::from_secs(2));
        if !just_copied {
            self.copied_at = None;
        }
        if ui.button(if just_copied { "Copiado" } else { "Copiar endereço" }).clicked() {
            ui.ctx().copy_text(self.local_address.clone());
            self.copied_at = Some(Instant::now());
        }

        ui.add_space(8.0);
        ui.separator();
        ui.add_space(4.0);
        let fingerprint = self.identity().fingerprint();
        egui::Grid::new("identity").num_columns(2).spacing([18.0, 6.0]).min_row_height(22.0).show(ui, |ui| {
            theme::detail(ui, p, "Nome", &self.device_name);
            theme::detail(ui, p, "ID do dispositivo", &fingerprint.device_id().to_string());
            theme::detail(ui, p, "Impressão digital", &fingerprint.short());
        });

        ui.add_space(8.0);
        ui.label(
            theme::muted(
                "Passe o endereço para quem vai acessar este computador. Nenhuma conexão começa sem você aprovar aqui.",
                p,
            )
            .small(),
        );
    }

    fn connect_form(&mut self, ui: &mut egui::Ui, p: &Palette) {
        ui.spacing_mut().item_spacing.x = 10.0;
        theme::caption(ui, p, "ACESSAR OUTRO COMPUTADOR");
        ui.add_space(6.0);

        let busy = self.outgoing.is_some();
        let label = ui.label(theme::muted("Endereço do computador remoto", p));
        let field = ui.add_enabled(
            !busy,
            egui::TextEdit::singleline(&mut self.address)
                .hint_text("192.168.0.10")
                .font(egui::FontId::proportional(17.0))
                .margin(Margin::symmetric(12, 10))
                .desired_width(f32::INFINITY),
        );
        let field = field.labelled_by(label.id);
        let submitted = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));

        ui.add_enabled(!busy, egui::Checkbox::new(&mut self.want_control, "Pedir controle de mouse e teclado"));
        ui.add_space(4.0);

        if busy {
            let (status, cancel) = match self.outgoing.as_ref().map(|o| (&o.stage, o)) {
                Some((Stage::Connecting, out)) => (format!("Conectando a {}…", out.target), "Cancelar"),
                Some((_, out)) => (format!("Aguardando {} autorizar o acesso…", out.host_name), "Cancelar pedido"),
                None => (String::new(), "Cancelar"),
            };
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().size(18.0).color(p.accent));
                ui.label(RichText::new(status).color(p.text));
            });
            if let Some(fingerprint) = self.outgoing.as_ref().and_then(|o| o.fingerprint) {
                ui.label(
                    theme::muted(
                        format!("ID {} · impressão digital {}", fingerprint.device_id(), fingerprint.short()),
                        p,
                    )
                    .small(),
                );
            }
            if ui.button(cancel).clicked() {
                self.end_outgoing(None);
            }
        } else {
            let ready = !self.address.trim().is_empty();
            let clicked = ui
                .with_layout(Layout::top_down_justified(Align::Center), |ui| {
                    ui.add_enabled(ready, theme::primary_button(p, "Conectar").min_size(Vec2::new(0.0, 40.0))).clicked()
                })
                .inner;
            if clicked || (submitted && ready) {
                self.connect();
            }
        }

        if let Some(notice) = &self.notice {
            ui.add_space(4.0);
            egui::Frame::new()
                .fill(p.control)
                .corner_radius(CornerRadius::same(8))
                .inner_margin(Margin::symmetric(12, 10))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(RichText::new(notice).color(p.text));
                });
        }

        ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
            ui.label(
                theme::muted(
                    "Por enquanto a conexão é direta, por endereço: os dois computadores precisam estar na mesma rede (ou com a porta encaminhada). Conexão por ID chega com o servidor de encontro.",
                    p,
                )
                .small(),
            );
        });
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
            .frame(egui::Frame::new().fill(p.alert_bg).inner_margin(Margin::symmetric(28, 8)))
            .show_separator_line(false)
            .show(ui, |ui| {
                for session in &self.inbound {
                    ui.horizontal(|ui| {
                        theme::dot(ui, p.alert_text);
                        let verb = if session.permissions.control { "está controlando" } else { "está vendo" };
                        let secs = session.since.elapsed().as_secs();
                        ui.label(
                            RichText::new(format!(
                                "{} {verb} este computador · {:02}:{:02}",
                                session.peer.name,
                                secs / 60,
                                secs % 60
                            ))
                            .font(theme::semibold(14.5))
                            .color(p.alert_text),
                        );
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
            .corner_radius(CornerRadius::same(16))
            .inner_margin(Margin::same(28));
        egui::Modal::new(egui::Id::unique("access-request")).frame(frame).show(ctx, |ui| {
            ui.set_width(460.0);
            theme::caption(ui, p, "PEDIDO DE ACESSO");
            let wants = if peer.wants.control { "ver e controlar" } else { "ver a tela de" };
            ui.label(
                RichText::new(format!("{} quer {wants} este computador", peer.name))
                    .font(theme::semibold(21.0))
                    .color(p.text),
            );
            ui.add_space(4.0);
            egui::Grid::new("request").num_columns(2).spacing([18.0, 6.0]).min_row_height(22.0).show(ui, |ui| {
                theme::detail(ui, p, "ID do dispositivo", &peer.fingerprint.device_id().to_string());
                theme::detail(ui, p, "Impressão digital", &peer.fingerprint.short());
                theme::detail(ui, p, "Endereço de rede", &peer.addr.to_string());
            });
            ui.add_space(4.0);
            egui::Frame::new()
                .fill(p.alert_bg)
                .corner_radius(CornerRadius::same(8))
                .inner_margin(Margin::symmetric(12, 10))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(
                        RichText::new(
                            "Só aceite se você conhece quem está pedindo e estava esperando por isso. Bancos, lojas e suporte técnico de verdade não pedem acesso ao seu computador por telefone.",
                        )
                        .color(p.alert_text),
                    );
                });
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Recusar").clicked() {
                    answer = Some(None);
                }
                ui.label(theme::muted(format!("expira em {remaining} s"), p).small());
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if peer.wants.control {
                        if ui.add(theme::primary_button(p, "Permitir controle")).clicked() {
                            answer = Some(Some(Permissions::FULL));
                        }
                        if ui.button("Somente ver").clicked() {
                            answer = Some(Some(Permissions::VIEW_ONLY));
                        }
                    } else if ui.add(theme::primary_button(p, "Permitir ver")).clicked() {
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
            .frame(egui::Frame::new().fill(p.surface).inner_margin(Margin::symmetric(16, 8)))
            .show_separator_line(false)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    theme::dot(ui, p.accent);
                    ui.label(RichText::new(&out.host_name).font(theme::semibold(14.5)).color(p.text));
                    if let Some(fingerprint) = out.fingerprint {
                        ui.label(theme::muted(format!("ID {}", fingerprint.device_id()), p).small());
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.button("Encerrar").clicked() {
                            end = true;
                        }
                        let mode = if permissions.control { "Controle ativo" } else { "Somente visualização" };
                        ui.label(RichText::new(mode).color(p.text));
                        ui.label(
                            theme::muted(
                                format!(
                                    "{}×{} · {:.0} qps · {:.1} Mbit/s",
                                    out.size[0], out.size[1], out.stats.fps, out.stats.mbps
                                ),
                                p,
                            )
                            .small(),
                        );
                    });
                });
            });
        if end {
            self.end_outgoing(None);
        }
    }

    fn remote_screen(&mut self, ui: &mut egui::Ui) {
        let Some(out) = &mut self.outgoing else { return };
        let Stage::Live { permissions } = out.stage else { return };
        egui::CentralPanel::default().frame(egui::Frame::new().fill(Color32::from_rgb(9, 10, 10))).show(ui, |ui| {
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

/// IP local pelo qual este computador sai para a rede. Um socket UDP
/// "conectado" não envia nenhum pacote; só faz o sistema escolher a rota.
fn local_ip() -> Option<IpAddr> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.connect((Ipv4Addr::new(192, 0, 2, 1), 9)).ok()?;
    Some(socket.local_addr().ok()?.ip())
}

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
