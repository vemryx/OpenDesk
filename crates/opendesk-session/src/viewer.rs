use crate::{Wake, describe_close};
use anyhow::{Context, Result, bail};
use opendesk_codec::decode_packet;
use opendesk_net::{Fingerprint, Node};
use opendesk_proto::PROTOCOL_VERSION;
use opendesk_proto::control::{Decision, DenyReason, DisplayDesc, Hello, HostMsg, InputEvent, Permissions, ViewerMsg};
use opendesk_proto::video::FramePacket;
use opendesk_proto::wire::{MAX_VIDEO_FRAME, read_frame, recv_msg, send_msg};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Instant;
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

#[derive(Debug, Clone)]
pub enum ViewerEvent {
    /// Conectado e autenticado; aguardando a pessoa no host decidir.
    Connected {
        host_name: String,
        fingerprint: Fingerprint,
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
}

impl Viewer {
    /// Começa a conectar em `addr`. Os eventos chegam pelo canal devolvido.
    pub fn connect(
        runtime: &tokio::runtime::Handle,
        node: Arc<Node>,
        addr: SocketAddr,
        device_name: String,
        wants: Permissions,
        wake: Wake,
    ) -> (Self, mpsc::UnboundedReceiver<ViewerEvent>) {
        let frame = Arc::new(Mutex::new(SharedFrame::default()));
        let (commands, command_rx) = mpsc::unbounded_channel();
        let (events, event_rx) = mpsc::unbounded_channel();
        let cancel = Arc::new(Notify::new());

        let task = Task { node, addr, device_name, wants, frame: frame.clone(), events, wake };
        let cancelled = cancel.clone();
        runtime.spawn(async move {
            let reason = tokio::select! {
                result = task.run(command_rx) => result.unwrap_or_else(|err| format!("{err:#}")),
                _ = cancelled.notified() => "Sessão encerrada.".to_string(),
            };
            task.emit(ViewerEvent::Closed(reason));
        });
        (Self { frame, commands, cancel }, event_rx)
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
    addr: SocketAddr,
    device_name: String,
    wants: Permissions,
    frame: Arc<Mutex<SharedFrame>>,
    events: mpsc::UnboundedSender<ViewerEvent>,
    wake: Wake,
}

impl Task {
    fn emit(&self, event: ViewerEvent) {
        let _ = self.events.send(event);
        (self.wake)();
    }

    async fn run(&self, mut commands: mpsc::UnboundedReceiver<Command>) -> Result<String> {
        let peer = self.node.connect(self.addr, None).await?;
        let conn = &peer.conn;
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
        self.emit(ViewerEvent::Connected { host_name, fingerprint: peer.fingerprint });

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
