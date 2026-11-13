use crate::{Wake, describe_close};
use anyhow::{Context, Result, anyhow, bail};
use opendesk_codec::decode_packet;
use opendesk_net::{DeviceId, Fingerprint, KnownHosts, Node, PeerConn, Rendezvous};
use opendesk_proto::PROTOCOL_VERSION;
use opendesk_proto::control::{Decision, DenyReason, DisplayDesc, Hello, HostMsg, InputEvent, Permissions, ViewerMsg};
use opendesk_proto::video::FramePacket;
use opendesk_proto::wire::{MAX_VIDEO_FRAME, read_frame, recv_msg, send_msg};
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{Notify, mpsc};

/// Framebuffer remoto, compartilhado entre a tarefa de rede e a interface.
#[derive(Default)]
pub struct SharedFrame {
    pub width: usize,
    pub height: usize,
    /// RGBA opaco, `width * height * 4` bytes.
    pub rgba: Vec<u8>,
    /// Região alterada desde a última vez que a interface a consumiu:
    /// `[x0, y0, x1, y1)`.
    pub dirty: Option<[usize; 4]>,
    /// Aumenta a cada troca de resolução; a interface recria a textura.
    pub epoch: u64,
    pub frames: u64,
    pub bytes: u64,
    pub last_frame: Option<Instant>,
}

impl SharedFrame {
    /// Entrega e limpa a região alterada.
    pub fn take_dirty(&mut self) -> Option<[usize; 4]> {
        self.dirty.take()
    }
}

/// Por quanto tempo o visualizador abre caminho para o host ao conectar por ID.
const PUNCH_FOR: Duration = Duration::from_secs(8);
/// Limite para a corrida entre os endereços candidatos do host.
const CONNECT_LIMIT: Duration = Duration::from_secs(10);

/// Para onde conectar.
#[derive(Clone)]
pub enum Target {
    /// Um endereço informado à mão.
    Address(SocketAddr),
    /// Um ID, a ser localizado pelo servidor de encontro.
    Id { id: DeviceId, lookup: Arc<Lookup> },
}

/// O necessário para achar um dispositivo pelo ID.
pub struct Lookup {
    pub rendezvous: Rendezvous,
    pub stun_servers: Vec<String>,
    pub known: KnownHosts,
}

impl Lookup {
    /// Monta a busca por ID a partir da configuração do ambiente (ver
    /// [`opendesk_net::rendezvous_url`]). `None` se não há servidor de
    /// encontro configurado: nesse caso só existe conexão por endereço.
    pub fn from_env(data_dir: &std::path::Path) -> Result<Option<Arc<Self>>> {
        let Some(url) = opendesk_net::rendezvous_url() else { return Ok(None) };
        Ok(Some(Arc::new(Self {
            rendezvous: Rendezvous::new(&url)?,
            stun_servers: opendesk_net::DEFAULT_STUN_SERVERS.iter().map(|s| s.to_string()).collect(),
            known: KnownHosts::load(data_dir),
        })))
    }
}

/// Por onde os dados da sessão estão passando. Em todos os casos a conexão é
/// direta entre os dois computadores e cifrada de ponta a ponta.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// Endereço digitado à mão.
    Address,
    /// Encontrado por ID, na mesma rede local.
    Lan,
    /// Encontrado por ID, atravessando a internet (NAT perfurado).
    Internet,
}

impl Route {
    pub fn label(self) -> &'static str {
        match self {
            Route::Address => "Direta, por endereço",
            Route::Lan => "Direta, rede local",
            Route::Internet => "Direta, ponto a ponto",
        }
    }
}

#[derive(Debug, Clone)]
pub enum ViewerEvent {
    /// Conectado e autenticado; aguardando a pessoa no host decidir.
    Connected {
        host_name: String,
        fingerprint: Fingerprint,
        route: Route,
    },
    Granted {
        permissions: Permissions,
        display: DisplayDesc,
    },
    Denied(DenyReason),
    /// A sessão terminou (ou nem chegou a começar). Sempre o último evento.
    Closed(String),
}

enum Command {
    Input(InputEvent),
    Keyframe,
}

#[derive(Clone)]
pub struct InputSender(mpsc::UnboundedSender<Command>);

impl InputSender {
    pub fn send(&self, event: InputEvent) {
        let _ = self.0.send(Command::Input(event));
    }
}

/// Uma sessão de saída. Descartar o valor encerra a sessão.
pub struct Viewer {
    pub frame: Arc<Mutex<SharedFrame>>,
    commands: mpsc::UnboundedSender<Command>,
    cancel: Arc<Notify>,
    link: Arc<Mutex<Option<quinn::Connection>>>,
}

impl Viewer {
    /// Começa a conectar a `target`. Os eventos chegam pelo canal devolvido.
    pub fn connect(
        runtime: &tokio::runtime::Handle,
        node: Arc<Node>,
        target: Target,
        device_name: String,
        wants: Permissions,
        wake: Wake,
    ) -> (Self, mpsc::UnboundedReceiver<ViewerEvent>) {
        let frame = Arc::new(Mutex::new(SharedFrame::default()));
        let (commands, command_rx) = mpsc::unbounded_channel();
        let (events, event_rx) = mpsc::unbounded_channel();
        let cancel = Arc::new(Notify::new());

        let link = Arc::new(Mutex::new(None));
        let task = Task { node, target, device_name, wants, frame: frame.clone(), events, wake, link: link.clone() };
        let cancelled = cancel.clone();
        runtime.spawn(async move {
            let reason = tokio::select! {
                result = task.run(command_rx) => result.unwrap_or_else(|err| format!("{err:#}")),
                _ = cancelled.notified() => "Sessão encerrada.".to_string(),
            };
            task.emit(ViewerEvent::Closed(reason));
        });
        (Self { frame, commands, cancel, link }, event_rx)
    }

    /// Latência de ida e volta medida pelo QUIC, quando já há conexão.
    pub fn rtt(&self) -> Option<Duration> {
        self.link.lock().unwrap().as_ref().map(quinn::Connection::rtt)
    }

    pub fn send_input(&self, event: InputEvent) {
        let _ = self.commands.send(Command::Input(event));
    }

    /// Remetente de entrada que pode ser levado para outra thread (por
    /// exemplo, a do gancho de teclado).
    pub fn input_sender(&self) -> InputSender {
        InputSender(self.commands.clone())
    }

    pub fn request_keyframe(&self) {
        let _ = self.commands.send(Command::Keyframe);
    }

    pub fn disconnect(&self) {
        self.cancel.notify_one();
    }
}

impl Drop for Viewer {
    fn drop(&mut self) {
        self.disconnect();
    }
}

struct Task {
    node: Arc<Node>,
    target: Target,
    device_name: String,
    wants: Permissions,
    frame: Arc<Mutex<SharedFrame>>,
    events: mpsc::UnboundedSender<ViewerEvent>,
    wake: Wake,
    link: Arc<Mutex<Option<quinn::Connection>>>,
}

impl Task {
    fn emit(&self, event: ViewerEvent) {
        let _ = self.events.send(event);
        (self.wake)();
    }

    /// Chega até o host: direto no endereço, ou pelo ID via servidor de
    /// encontro, com perfuração de NAT.
    async fn establish(&self) -> Result<(PeerConn, Route)> {
        let (id, lookup) = match &self.target {
            Target::Address(addr) => return Ok((self.node.connect(*addr, None).await?, Route::Address)),
            Target::Id { id, lookup } => (*id, lookup),
        };

        let (mine, _) = self.node.candidates(&lookup.stun_servers).await;
        let answer = lookup.rendezvous.dial(id, self.node.identity().fingerprint(), &mine).await?;

        // Primeiro contato fixa a identidade; depois disso ela não pode mudar.
        let pinned = lookup.known.get(id);
        if let Some(pinned) = pinned
            && pinned != answer.fingerprint
        {
            bail!(
                "A identidade do computador {id} mudou desde a última conexão (era {}, agora é {}). \
                 Isso acontece se o OpenDesk foi reinstalado lá, mas também se alguém estiver se passando por ele. \
                 Confirme com a pessoa por outro meio antes de continuar.",
                pinned.short(),
                answer.fingerprint.short()
            );
        }
        if answer.candidates.is_empty() {
            bail!("O computador remoto não informou nenhum endereço para conexão.");
        }

        self.node.punch(&answer.candidates, PUNCH_FOR);
        let (peer, addr) =
            self.node.connect_any(&answer.candidates, Some(answer.fingerprint), CONNECT_LIMIT).await.map_err(
                |err| {
                    tracing::debug!("conexão direta com {id} falhou: {err:#}");
                    anyhow!(
                        "Não foi possível abrir uma conexão direta com esse computador. \
                     A rede de um dos lados bloqueia conexões ponto a ponto, e o relay ainda não está disponível."
                    )
                },
            )?;
        if pinned.is_none()
            && let Err(err) = lookup.known.remember(id, peer.fingerprint)
        {
            tracing::warn!("não foi possível guardar a identidade de {id}: {err}");
        }
        let local = match addr.ip() {
            IpAddr::V4(ip) => ip.is_private() || ip.is_loopback() || ip.is_link_local(),
            IpAddr::V6(ip) => ip.is_loopback(),
        };
        Ok((peer, if local { Route::Lan } else { Route::Internet }))
    }

    async fn run(&self, mut commands: mpsc::UnboundedReceiver<Command>) -> Result<String> {
        let (peer, route) = self.establish().await?;
        let conn = &peer.conn;
        *self.link.lock().unwrap() = Some(conn.clone());
        // Se esta tarefa for cancelada, avisa o host em vez de deixá-lo
        // esperar o tempo limite.
        let _close_on_exit = CloseOnDrop(conn.clone());

        let (mut tx, mut rx) = conn.open_bi().await.context("abrir canal de controle")?;
        send_msg(
            &mut tx,
            &ViewerMsg::Hello(Hello {
                protocol: PROTOCOL_VERSION,
                device_name: self.device_name.clone(),
                app_version: env!("CARGO_PKG_VERSION").into(),
                wants: self.wants,
            }),
        )
        .await?;

        let HostMsg::Welcome { device_name, .. } = recv_msg(&mut rx).await.context("aguardar o host")? else {
            bail!("resposta inesperada do host");
        };
        let host_name: String = device_name.chars().filter(|c| !c.is_control()).take(64).collect();
        self.emit(ViewerEvent::Connected { host_name, fingerprint: peer.fingerprint, route });

        match recv_msg(&mut rx).await {
            Ok(HostMsg::Decision(Decision::Granted { permissions, display })) => {
                self.emit(ViewerEvent::Granted { permissions, display });
            }
            Ok(HostMsg::Decision(Decision::Denied { reason })) => {
                self.emit(ViewerEvent::Denied(reason));
                return Ok(match reason {
                    DenyReason::Rejected => "O pedido de conexão foi recusado.",
                    DenyReason::Timeout => "Ninguém respondeu ao pedido de conexão.",
                    DenyReason::VersionMismatch => "As versões do OpenDesk nos dois lados são incompatíveis.",
                    DenyReason::CaptureUnavailable => "O computador remoto não conseguiu capturar a tela.",
                }
                .into());
            }
            Ok(_) => bail!("resposta inesperada do host"),
            Err(_) => {
                return Ok(match conn.close_reason() {
                    Some(err) => describe_close(&err),
                    None => "O computador remoto fechou a conexão.".into(),
                });
            }
        }

        let video = self.receive_video(conn);

        let outgoing = async {
            while let Some(mut command) = commands.recv().await {
                // Rajadas de movimento do mouse: só a posição mais recente
                // interessa. O resto vai em ordem, sem perder nada.
                while let Ok(next) = commands.try_recv() {
                    let both_moves = matches!(command, Command::Input(InputEvent::MouseMove { .. }))
                        && matches!(next, Command::Input(InputEvent::MouseMove { .. }));
                    if !both_moves {
                        send_command(&mut tx, &command).await?;
                    }
                    command = next;
                }
                send_command(&mut tx, &command).await?;
            }
            // O dono da sessão foi descartado.
            let _ = send_msg(&mut tx, &ViewerMsg::Bye).await;
            anyhow::Ok(())
        };

        let reason = tokio::select! {
            err = video => match conn.close_reason() {
                Some(closed) => describe_close(&closed),
                None => format!("{err:#}"),
            },
            result = outgoing => match result {
                Ok(()) => "Sessão encerrada.".into(),
                Err(err) => format!("{err:#}"),
            },
            closed = conn.closed() => describe_close(&closed),
        };
        Ok(reason)
    }

    /// Recebe e aplica quadros até a conexão cair; devolve o erro que a parou.
    async fn receive_video(&self, conn: &quinn::Connection) -> anyhow::Error {
        let mut stream = match conn.accept_uni().await.context("aguardar canal de vídeo") {
            Ok(stream) => stream,
            Err(err) => return err,
        };
        loop {
            let step = async {
                let bytes = read_frame(&mut stream, MAX_VIDEO_FRAME).await.context("receber quadro")?;
                let packet = FramePacket::decode(&bytes)?;
                let patches = tokio::task::block_in_place(|| decode_packet(&packet))?;
                self.apply(&packet, &patches, bytes.len());
                anyhow::Ok(())
            };
            if let Err(err) = step.await {
                return err;
            }
            (self.wake)();
        }
    }

    fn apply(&self, packet: &FramePacket, patches: &[opendesk_codec::DecodedPatch], wire_bytes: usize) {
        let (width, height) = (packet.width as usize, packet.height as usize);
        let mut frame = self.frame.lock().unwrap();
        if frame.width != width || frame.height != height {
            frame.width = width;
            frame.height = height;
            frame.rgba = vec![0; width * height * 4];
            frame.epoch += 1;
            frame.dirty = None;
        }
        for patch in patches {
            for row in 0..patch.h {
                let dst = ((patch.y + row) * width + patch.x) * 4;
                let src = row * patch.w * 4;
                frame.rgba[dst..dst + patch.w * 4].copy_from_slice(&patch.rgba[src..src + patch.w * 4]);
            }
            let (x1, y1) = (patch.x + patch.w, patch.y + patch.h);
            frame.dirty = Some(match frame.dirty {
                Some([a, b, c, d]) => [a.min(patch.x), b.min(patch.y), c.max(x1), d.max(y1)],
                None => [patch.x, patch.y, x1, y1],
            });
        }
        frame.frames += 1;
        frame.bytes += wire_bytes as u64;
        frame.last_frame = Some(Instant::now());
    }
}

async fn send_command(tx: &mut quinn::SendStream, command: &Command) -> Result<()> {
    let msg = match command {
        Command::Input(event) => ViewerMsg::Input(*event),
        Command::Keyframe => ViewerMsg::RequestKeyframe,
    };
    send_msg(tx, &msg).await.context("enviar comando")
}

struct CloseOnDrop(quinn::Connection);

impl Drop for CloseOnDrop {
    fn drop(&mut self) {
        self.0.close(0u32.into(), b"viewer");
    }
}
